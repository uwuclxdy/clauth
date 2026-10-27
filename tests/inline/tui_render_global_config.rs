//! Config-tab row geometry. Every blurred row's value starts at the same
//! column (the Config tab is a tight chip group); cycle options are
//! bare labels on 2-space gaps with the active option bracketed only on focus;
//! an on/off boolean renders as a toggle, not a 2-option cycle.

use super::*;
use ratatui::style::Modifier;

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Wide enough that no row wraps: these pins are about one line's geometry.
const WIDE: usize = 200;

fn row(key: &str, options: &[(&str, bool)], selected: bool) -> String {
    let lines = cycle_row(Span::raw("  "), key, options, selected, WIDE);
    assert_eq!(lines.len(), 1, "a wide pane holds the row on one line");
    line_text(&lines[0])
}

/// [`detail_row`] in a pane wide enough that the row is one line.
fn one_row(
    r: GlobalConfigRow,
    selected: bool,
    rows: RowState,
    tunables: RowTunables,
    editing: Option<&InputState>,
) -> Line<'static> {
    let mut lines = detail_row(r, selected, rows, tunables, editing, WIDE);
    assert_eq!(lines.len(), 1, "a wide pane holds the row on one line");
    lines.remove(0)
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
        walk_order: WalkOrder::Chain,
        spend_budget: false,
        switch_off_when_budget_spent: true,
        preemptive: false,
        preserve_non_login_keys: true,
        sync_mcp_logins: true,
        auto_update: true,
        refresh_spent: true,
        auto_start_queue: true,
        any_auto_start: true,
        reset_display: ResetDisplay::Relative,
        clock_format: ClockFormat::H24,
        home_tab: HomeTab::Overview,
    }
}

fn tunables() -> RowTunables {
    RowTunables {
        refresh_interval_ms: 60_000,
        weekly_pct: 95.0,
        burn_floor_pct: 98.0,
        burn_horizon_ms: 60_000,
        default_divergence: None,
        context_nudge_tokens: None,
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
        let line = line_text(&one_row(r, false, toggles(), tunables(), None));
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
    let line = one_row(GlobalConfigRow::Theme, true, toggles(), tunables(), None);
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

/// The appearance band's fourth row: `home tab` lists the eight tab names as
/// bare chips and opens its value at the shared column, like every other row.
#[test]
fn home_tab_renders_in_the_appearance_band_at_the_shared_value_column() {
    let mut found = false;
    for r in GLOBAL_CONFIG_ROWS {
        if r.band() != "appearance" {
            continue;
        }
        let line = line_text(&one_row(r, false, toggles(), tunables(), None));
        if line.contains("home tab") {
            found = true;
            for name in [
                "overview", "usage", "tokens", "setup", "fallback", "config", "status", "services",
            ] {
                assert!(line.contains(name), "the home tab row lists {name}: {line}");
            }
            assert_eq!(
                value_col("home tab", &line),
                2 + KEY_W + KEY_GUTTER,
                "the home tab value opens at the shared column: {line}"
            );
        }
    }
    assert!(found, "the appearance band holds the home tab row");
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
        offset: std::cell::Cell::new(0),
    };
    for rendered in [
        line_text(&weekly_edit_line(Span::raw("  "), &input, 80)),
        line_text(&refresh_edit_line(Span::raw("  "), &input, 80)),
        line_text(&context_nudge_edit_line(Span::raw("  "), &input, 80)),
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
    let on = line_text(&one_row(
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
    let off_line = line_text(&one_row(
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

/// `refresh spent` is a pure on/off boolean — a toggle (`─●` / `○─`),
/// not a 2-option cycle row (`[on]  off`).
#[test]
fn refresh_spent_renders_as_a_toggle_not_a_cycle() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let on = line_text(&one_row(
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
    let off_line = line_text(&one_row(
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
/// queue row renders as a disabled row (whole content faint, knob
/// included) — it must never read as an armed setting. One opted-in account
/// makes it a live toggle again.
#[test]
fn auto_start_queue_dims_when_no_account_opts_in() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut none_opted = toggles();
    none_opted.any_auto_start = false;
    let dimmed = one_row(
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

    let live = one_row(
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
/// It renders as a disabled row (whole content faint) so it never
/// reads as an armed setting; flip the toggle on and it becomes a live cycle.
#[test]
fn money_spent_dims_when_spend_budget_is_off() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dimmed = one_row(
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
    let live = line_text(&one_row(
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
    let off = line_text(&one_row(
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
    assert_eq!(
        off, "  refresh            15s  30s  60s  90s  120s  300s   default: 90s",
        "off-default carries it 3 cells out"
    );
    let off_line = one_row(
        GlobalConfigRow::RefreshInterval,
        false,
        toggles(),
        RowTunables {
            refresh_interval_ms: 30_000,
            weekly_pct: 98.0,
            ..tunables()
        },
        None,
    );
    for word in ["default:", "90s"] {
        let span = off_line
            .spans
            .iter()
            .rev()
            .find(|s| s.content == word)
            .unwrap_or_else(|| panic!("the reminder's `{word}` renders"));
        assert_eq!(span.style, theme::faint(), "`{word}` is faint");
    }
    let default = line_text(&one_row(
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

// ── context nudge ────────────────────────────────────────────────────────────

/// The nudge row is a five-way cycle: `off` first (the shipped default), then
/// the four presets. `None` brackets `off`; a preset value brackets its chip.
#[test]
fn context_nudge_cycle_line_renders_off_then_presets() {
    let off = line_text(&one_row(
        GlobalConfigRow::ContextNudge,
        true,
        toggles(),
        tunables(), // None
        None,
    ));
    assert!(off.contains("[off]"), "None brackets off: {off}");
    for label in ["300k", "400k", "600k", "900k"] {
        assert!(off.contains(label), "all presets render: {off}");
    }

    let mut set = tunables();
    set.context_nudge_tokens = Some(300_000);
    let low = line_text(&one_row(
        GlobalConfigRow::ContextNudge,
        true,
        toggles(),
        set,
        None,
    ));
    assert!(low.contains("[300k]"), "the live preset brackets: {low}");
    assert!(
        !low.contains("[off]"),
        "off stays bare while a preset is live: {low}"
    );

    let mut set = tunables();
    set.context_nudge_tokens = Some(900_000);
    let high = line_text(&one_row(
        GlobalConfigRow::ContextNudge,
        true,
        toggles(),
        set,
        None,
    ));
    assert!(high.contains("[900k]"), "the top preset brackets: {high}");
}

/// A custom value matches no preset, so the real threshold is appended in
/// `ACCENT` instead of mis-bracketing the nearest chip — the refresh row's
/// custom-value append. The shared display form: `{n}k`, `{n}M`, or plain
/// tokens.
#[test]
fn context_nudge_custom_value_appends_in_accent_without_bracketing_a_preset() {
    let mut set = tunables();
    set.context_nudge_tokens = Some(450_000);
    let line = one_row(GlobalConfigRow::ContextNudge, true, toggles(), set, None);
    assert!(
        !line_text(&line).contains('['),
        "no preset may bracket: {}",
        line_text(&line)
    );
    let custom = line
        .spans
        .iter()
        .find(|s| s.content.contains("450k"))
        .expect("the custom value appends in k form");
    assert_eq!(
        custom.style.fg,
        theme::accent().fg,
        "the custom append renders accent: {:?}",
        custom.style
    );

    let mut set = tunables();
    set.context_nudge_tokens = Some(450_500);
    let plain = line_text(&one_row(
        GlobalConfigRow::ContextNudge,
        false,
        toggles(),
        set,
        None,
    ));
    assert!(
        plain.contains("450500"),
        "an indivisible value appends as plain tokens: {plain}"
    );

    let mut set = tunables();
    set.context_nudge_tokens = Some(1_000_000);
    let million = line_text(&one_row(
        GlobalConfigRow::ContextNudge,
        false,
        toggles(),
        set,
        None,
    ));
    assert!(
        million.contains("1M"),
        "an exact million appends in M form: {million}"
    );
}

/// The default is off, so the faint ` default: off` reminder rides the row only
/// while a threshold is set — the refresh row's off-default idiom.
#[test]
fn context_nudge_default_reminder_appears_only_when_set() {
    let mut set = tunables();
    set.context_nudge_tokens = Some(600_000);
    let on = line_text(&one_row(
        GlobalConfigRow::ContextNudge,
        false,
        toggles(),
        set,
        None,
    ));
    assert!(on.contains("default: off"), "a set value carries it: {on}");

    let off = line_text(&one_row(
        GlobalConfigRow::ContextNudge,
        false,
        toggles(),
        tunables(), // None = the default
        None,
    ));
    assert!(
        !off.contains("default:"),
        "the default carries no reminder: {off}"
    );
}

/// The hint states what the row does per live value, byte-pinned: off names
/// the absence of a nudge, a set threshold names the number it crosses.
#[test]
fn context_nudge_hint_tracks_the_live_value() {
    assert_eq!(
        row_hint(GlobalConfigRow::ContextNudge, toggles(), tunables()).as_deref(),
        Some("no context nudge is sent"),
    );
    let mut set = tunables();
    set.context_nudge_tokens = Some(600_000);
    assert_eq!(
        row_hint(GlobalConfigRow::ContextNudge, toggles(), set).as_deref(),
        Some("tells a running session when its context usage crosses 600k"),
    );
}

/// The editor takes raw tokens (a trailing `k` lives inside the buffer), so an
/// out-of-range or non-numeric buffer renders DANGER through `value_caret` —
/// pinned at the span level since a text dump cannot see color.
#[test]
fn context_nudge_edit_line_marks_invalid_buffer_danger() {
    let invalid = InputState::new("49999");
    let line = context_nudge_edit_line(Span::raw("  "), &invalid, 80);
    let buffer = line
        .spans
        .iter()
        .find(|s| s.content == "49999")
        .expect("the typed buffer renders");
    assert_eq!(buffer.style.fg, theme::danger().fg, "{:?}", buffer.style);

    let valid = InputState::new("600k");
    let line = context_nudge_edit_line(Span::raw("  "), &valid, 80);
    let buffer = line
        .spans
        .iter()
        .find(|s| s.content == "600k")
        .expect("the typed buffer renders");
    assert_eq!(buffer.style.fg, theme::body().fg, "{:?}", buffer.style);
}

/// The invalid tooltip renders leader + reason both in DANGER (the house
/// Invalid-input treatment); the valid range tooltip stays in the faint help
/// shape. Both name the same range: `50k-2M tokens`.
#[test]
fn context_nudge_range_tooltip_marks_invalid_input_danger() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let invalid = InputState::new("49999");
    let lines = context_nudge_range_tooltip(&invalid, 40);
    for line in &lines {
        for span in &line.spans {
            assert_eq!(
                span.style.fg,
                theme::danger().fg,
                "leader and reason both danger: {span:?}"
            );
        }
    }
    let text: String = lines.iter().map(line_text).collect();
    assert!(text.contains("50k-2M tokens"), "{text}");

    let valid = InputState::new("600k");
    for line in context_nudge_range_tooltip(&valid, 40) {
        for span in line.spans {
            assert_ne!(
                span.style.fg,
                theme::danger().fg,
                "a valid buffer keeps the tooltip out of danger: {span:?}"
            );
        }
    }
}

/// The row belongs to the scheduler band, between the refresh rows and the
/// auto-start queue — a nudge is cadence behavior, not a switch rule.
#[test]
fn context_nudge_sits_in_the_scheduler_band_after_the_refresh_rows() {
    assert_eq!(GlobalConfigRow::ContextNudge.band(), "scheduler");
    let pos = |row: GlobalConfigRow| {
        GLOBAL_CONFIG_ROWS
            .iter()
            .position(|r| *r == row)
            .expect("row in the config list")
    };
    let nudge = pos(GlobalConfigRow::ContextNudge);
    assert!(nudge > pos(GlobalConfigRow::RefreshInterval));
    assert!(nudge > pos(GlobalConfigRow::RefreshSpentAccounts));
    assert!(nudge < pos(GlobalConfigRow::AutoStartQueue));
}

/// The row belongs to the scheduler band, between `rotation` and the
/// auto-switch band — an update cadence, not a switch rule.
#[test]
fn auto_update_sits_in_the_scheduler_band_after_rotation() {
    assert_eq!(GlobalConfigRow::AutoUpdate.band(), "scheduler");
    let pos = |row: GlobalConfigRow| {
        GLOBAL_CONFIG_ROWS
            .iter()
            .position(|r| *r == row)
            .expect("row in the config list")
    };
    let auto = pos(GlobalConfigRow::AutoUpdate);
    assert!(auto > pos(GlobalConfigRow::PreemptiveRotation));
    assert!(auto < pos(GlobalConfigRow::WeeklyThreshold));
}

/// The row is a plain toggle on both tiers (never dimmed, never a cycle) and
/// its hint names the env override on and the silence off — the exact copy,
/// since both strings are provisional pending cloudy's equality pick.
#[test]
fn auto_update_renders_the_toggle_glyphs_and_exact_hints() {
    let mut off_state = toggles();
    off_state.auto_update = false;
    {
        let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
        let on = line_text(&one_row(
            GlobalConfigRow::AutoUpdate,
            false,
            toggles(),
            tunables(),
            None,
        ));
        assert!(
            on.contains("─●"),
            "full-tier on renders the slide switch: {on}"
        );
        let off = line_text(&one_row(
            GlobalConfigRow::AutoUpdate,
            false,
            off_state,
            tunables(),
            None,
        ));
        assert!(
            off.contains("○─"),
            "full-tier off renders the hollow knob: {off}"
        );
    }
    {
        let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);
        let on = line_text(&one_row(
            GlobalConfigRow::AutoUpdate,
            false,
            toggles(),
            tunables(),
            None,
        ));
        assert!(
            on.contains("[on]"),
            "compatible-tier on renders the bracket: {on}"
        );
        let off = line_text(&one_row(
            GlobalConfigRow::AutoUpdate,
            false,
            off_state,
            tunables(),
            None,
        ));
        assert!(
            off.contains("[off]"),
            "compatible-tier off renders the bracket: {off}"
        );
    }

    let hint_on = row_hint(GlobalConfigRow::AutoUpdate, toggles(), tunables()).expect("hint");
    assert_eq!(
        hint_on,
        "checks for updates at launch, unless CLAUTH_NO_UPDATE=1"
    );
    let hint_off = row_hint(GlobalConfigRow::AutoUpdate, off_state, tunables()).expect("hint");
    assert_eq!(hint_off, "no update checks");
}

/// The row sits right after `on mismatch`, the other row about what clauth
/// does with the live credential, at the head of the scheduler band.
#[test]
fn non_login_keys_sits_after_on_mismatch() {
    assert_eq!(GlobalConfigRow::PreserveNonLoginKeys.band(), "scheduler");
    let pos = |row: GlobalConfigRow| {
        GLOBAL_CONFIG_ROWS
            .iter()
            .position(|r| *r == row)
            .expect("row in the config list")
    };
    assert_eq!(
        pos(GlobalConfigRow::PreserveNonLoginKeys),
        pos(GlobalConfigRow::DivergenceDefault) + 1
    );
}

/// A plain toggle on both tiers, never dimmed, and its hints name what on
/// keeps and what off drops, exact copy.
#[test]
fn non_login_keys_renders_the_toggle_and_exact_hints() {
    let mut off_state = toggles();
    off_state.preserve_non_login_keys = false;
    {
        let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
        let on = line_text(&one_row(
            GlobalConfigRow::PreserveNonLoginKeys,
            false,
            toggles(),
            tunables(),
            None,
        ));
        assert!(on.contains("non-login keys"), "the key reads: {on}");
        assert!(
            on.contains("─●"),
            "full-tier on renders the slide switch: {on}"
        );
        let off = line_text(&one_row(
            GlobalConfigRow::PreserveNonLoginKeys,
            false,
            off_state,
            tunables(),
            None,
        ));
        assert!(
            off.contains("○─"),
            "full-tier off renders the hollow knob: {off}"
        );
    }
    {
        let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);
        let off = line_text(&one_row(
            GlobalConfigRow::PreserveNonLoginKeys,
            false,
            off_state,
            tunables(),
            None,
        ));
        assert!(
            off.contains("[off]"),
            "compatible-tier off renders the bracket: {off}"
        );
    }

    assert_eq!(
        row_hint(GlobalConfigRow::PreserveNonLoginKeys, toggles(), tunables()).expect("hint"),
        "keep a /design-login with the account it was made on"
    );
    assert_eq!(
        row_hint(GlobalConfigRow::PreserveNonLoginKeys, off_state, tunables()).expect("hint"),
        "a /design-login is dropped at the next switch, and outside macOS at any relink"
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
/// switch mode, so they render as disabled rows (whole content faint)
/// while burn-aware is off, and become live cycles once it is on.
#[test]
fn burn_tunables_dim_when_burn_aware_is_off() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    for r in [GlobalConfigRow::BurnFloor, GlobalConfigRow::BurnHorizon] {
        let dimmed = one_row(r, false, toggles(), tunables(), None);
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
        let live = line_text(&one_row(r, true, on, tunables(), None));
        assert!(
            live.contains('['),
            "{r:?} burn-aware on: live + focused brackets the active preset: {live}"
        );
    }
}

// ── walk order (issue #86): a 3-option cycle beside switch mode ─────────────

/// `walk order` is a plain 3-option `cycle_row`; its hint states behavior
/// alone for each value, never restating the row's own value.
#[test]
fn walk_order_renders_as_a_cycle_with_every_hint_pinned() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let chain = toggles();
    let line = line_text(&one_row(
        GlobalConfigRow::WalkOrder,
        false,
        chain,
        tunables(),
        None,
    ));
    assert!(line.contains("walk order"), "{line}");
    assert!(line.contains("chain"), "{line}");
    assert!(
        line.contains("soonest weekly reset"),
        "the inactive option stays visible: {line}"
    );
    assert!(
        line.contains("most weekly headroom"),
        "the third option stays visible: {line}"
    );
    assert_eq!(
        row_hint(GlobalConfigRow::WalkOrder, chain, tunables()).as_deref(),
        Some("pick the next member with headroom by chain position"),
    );
    let soonest = RowState {
        walk_order: WalkOrder::SoonestWeeklyReset,
        ..toggles()
    };
    let soonest_line = line_text(&one_row(
        GlobalConfigRow::WalkOrder,
        false,
        soonest,
        tunables(),
        None,
    ));
    assert!(
        soonest_line.contains("soonest weekly reset"),
        "{soonest_line}"
    );
    assert_eq!(
        row_hint(GlobalConfigRow::WalkOrder, soonest, tunables()).as_deref(),
        Some(
            "spend the accepted member whose 7d window resets soonest, so less quota expires unspent"
        ),
    );
    let headroom = RowState {
        walk_order: WalkOrder::MostWeeklyHeadroom,
        ..toggles()
    };
    // Focus brackets the ACTIVE option, so the bracket pins which value the
    // row holds (a substring alone also matches an inactive option).
    let focused_soonest = line_text(&one_row(
        GlobalConfigRow::WalkOrder,
        true,
        soonest,
        tunables(),
        None,
    ));
    assert!(
        focused_soonest.contains("[soonest weekly reset]"),
        "{focused_soonest}"
    );
    let focused_headroom = line_text(&one_row(
        GlobalConfigRow::WalkOrder,
        true,
        headroom,
        tunables(),
        None,
    ));
    assert!(
        focused_headroom.contains("[most weekly headroom]"),
        "{focused_headroom}"
    );
    assert_eq!(
        row_hint(GlobalConfigRow::WalkOrder, headroom, tunables()).as_deref(),
        Some(
            "spend the accepted member whose 7d window is least used, so the week spreads across the chain"
        ),
    );
}

// ── preemptive rotation is live on every platform ────────────────────────────

/// The rotation lead is a clock margin against the running `claude`'s own
/// refresh threshold, which is platform-independent — so the `rotation` row is
/// an editable `cycle_row` everywhere, never a dimmed one, and its hint states
/// the behavior rather than a platform caveat.
#[test]
fn rotation_row_is_live_on_every_platform() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let row = one_row(
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

/// The header is a bold eyebrow whatever the cursor does; focus moves only its
/// tier, `TEXT_DIM` at rest to `TEXT` while the cursor sits in the band, and
/// never underlines it. Brightening a band with no row focused, or underlining
/// either state, is the bug this pins.
#[test]
fn band_header_brightens_only_the_focused_band() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let blurred = band_header("auto-switch", false);
    let focused = band_header("auto-switch", true);
    for (line, fg, state) in [
        (&blurred, crate::testutil::EYEBROW_AT_REST, "at rest"),
        (&focused, crate::testutil::EYEBROW_ACTIVE, "focused"),
    ] {
        assert_eq!(line_text(line), "AUTO-SWITCH", "eyebrows render uppercase");
        let style = line.spans[0].style;
        assert_eq!(style.fg, Some(fg), "the {state} eyebrow's tier");
        assert!(
            style.add_modifier.contains(Modifier::BOLD),
            "the {state} eyebrow is bold"
        );
        assert!(
            !style.add_modifier.contains(Modifier::UNDERLINED),
            "the {state} eyebrow is never underlined"
        );
    }
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
        let line = line_text(&one_row(
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
/// row is a disabled row until a clock shows — and live after.
#[test]
fn clock_row_dims_until_a_reset_renders_a_clock() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dimmed = one_row(
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
        let live = line_text(&one_row(
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

/// A 23-cell pane (a 27-col terminal) must keep the dim clock's current value
/// whole: it drops to its own line past the caret gutter instead of clipping
/// the trailing `h`.
#[test]
fn the_clock_notation_stays_whole_at_a_narrow_pane() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let rows = toggles(); // reset_display Relative -> the clock row is dim
    let lines = detail_row(
        GlobalConfigRow::ClockNotation,
        false,
        rows,
        tunables(),
        None,
        23,
    );
    let text: Vec<String> = lines.iter().map(line_text).collect();
    assert!(
        text.iter().any(|t| t.trim() == "24h"),
        "the whole `24h` drops to its own line at 23 cells: {text:?}"
    );
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

/// The Config tab with the cursor on `row`, that row's typed editor holding
/// `draft`.
fn config_typed_app(row: crate::tui::app::GlobalConfigRow, draft: &str) -> crate::tui::app::App {
    use crate::tui::app::{GLOBAL_CONFIG_ROWS, SeededInput, Tab};
    let mut app = crate::tui::app::App::new(crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == row)
        .expect("the row is listed");
    match row {
        crate::tui::app::GlobalConfigRow::RefreshInterval => {
            app.refresh_interval_draft = Some(SeededInput::new(draft));
        }
        crate::tui::app::GlobalConfigRow::WeeklyThreshold => {
            app.weekly_threshold_draft = Some(SeededInput::new(draft));
        }
        crate::tui::app::GlobalConfigRow::ContextNudge => {
            app.context_nudge_draft = Some(SeededInput::new(draft));
        }
        _ => panic!("the row has no typed editor"),
    }
    app
}

fn refresh_app(draft: Option<&str>) -> crate::tui::app::App {
    let mut app = config_typed_app(
        crate::tui::app::GlobalConfigRow::RefreshInterval,
        draft.unwrap_or(""),
    );
    if draft.is_none() {
        app.refresh_interval_draft = None;
    }
    app
}

/// A body text editor's caret that lands inside a drawn toast hides the native
/// cursor this frame (it would blink through the toast's glass); clear of the
/// stack, the caret shows on its cell. It also hides under an open modal. The
/// Config tab's `refresh` custom field ("45") sits at a fixed column, so three
/// stacked DANGER toasts cover it at every probe size.
#[test]
fn a_toast_over_a_body_editor_caret_hides_the_native_cursor() {
    use crate::tui::app::{Modal, ToastKind};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);

    let mut app = refresh_app(Some("45"));

    // Baseline: no toasts → the caret shows on the field.
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        term.backend().cursor_visible(),
        "the refresh caret shows with no toasts"
    );
    let baseline = term.backend().cursor_position();

    let wide = "x".repeat(53);
    for name in ["one", "two", "three"] {
        app.toast(ToastKind::Danger, format!("{name}\n{wide}\n{name} tail"));
    }

    // The caret sits under the stack at every probe size → no native cursor.
    for (w, h) in [(45u16, 14u16), (60u16, 12u16), (80u16, 24u16)] {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
        assert!(
            !term.backend().cursor_visible(),
            "[{w}x{h}] the body caret hides under a toast"
        );
    }

    // A single short toast anchored top-right stays clear of the caret: it
    // still lands on the field cell.
    let mut clear = refresh_app(Some("45"));
    clear.toast(ToastKind::Danger, "boom");
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &clear)).unwrap();
    assert!(
        term.backend().cursor_visible(),
        "the caret shows clear of a short toast"
    );
    assert_eq!(
        term.backend().cursor_position(),
        baseline,
        "the caret stays on the field cell"
    );

    // While a modal is open the whole frame is the blocker, so the body caret
    // must hide too instead of blinking on the modal.
    app.toasts.clear();
    app.open_modal(Modal::Help);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        !term.backend().cursor_visible(),
        "the refresh caret hides under an open modal"
    );
}

/// S10: a refresh buffer wider than its field scrolls inside the row: End
/// keeps the caret inside the pane right after the last digit, one ← moves it
/// one cell and leaves the row alone, Home shows the first digit again.
#[test]
fn an_over_wide_refresh_buffer_scrolls_inside_its_row() {
    use crate::testutil::{draw_caret, right_border, symbol_at};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = refresh_app(Some(&"1234567890".repeat(4)));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(45, 30)).unwrap();
    let draw =
        |f: &mut ratatui::Frame<'_>, app: &crate::tui::app::App| crate::tui::render::draw(f, app);

    let (end, buf) = draw_caret(&mut term, |f| draw(f, &app));
    let border = right_border(&buf, end.y);
    assert!(
        end.x + 2 <= border,
        "End stays inside the field ({end:?}, border {border})"
    );
    assert_eq!(
        symbol_at(&buf, end.x - 1, end.y),
        "0",
        "End follows the last digit"
    );
    let end_row = crate::testutil::buffer_rows(&buf)[end.y as usize].clone();

    app.refresh_interval_draft
        .as_mut()
        .expect("a draft")
        .input
        .left();
    let (left, buf) = draw_caret(&mut term, |f| draw(f, &app));
    assert_eq!(
        left,
        ratatui::layout::Position::new(end.x - 1, end.y),
        "one left moves one cell"
    );
    assert_eq!(
        crate::testutil::buffer_rows(&buf)[left.y as usize],
        end_row,
        "a move inside the window leaves the row alone"
    );

    app.refresh_interval_draft
        .as_mut()
        .expect("a draft")
        .input
        .home();
    let (home, buf) = draw_caret(&mut term, |f| draw(f, &app));
    assert_eq!(home.y, end.y, "Home stays on the field row");
    assert_eq!(
        symbol_at(&buf, home.x, home.y),
        "1",
        "Home sits on the first digit"
    );
}

/// S10 (cloudy's range rule, 2026-10-14): typing in `refresh` swaps the
/// preset run and its hint for the edit line and its range tooltip, and the
/// rows below move by exactly the height difference: at 28 columns the run
/// takes 3 lines and the hint 2 (edit form 1 + 1), at 45 the run 2 and the
/// hint 1, at 80 one each.
#[test]
fn the_refresh_editor_swaps_its_hint_for_its_range_while_typing() {
    let _home = crate::testutil::HomeSandbox::new();
    for (w, delta) in [(28u16, -3isize), (45, -1), (80, 0)] {
        let frame = |app: &crate::tui::app::App| {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, 100)).unwrap();
            term.draw(|f| crate::tui::render::draw(f, app)).unwrap();
            crate::testutil::buffer_rows(term.backend().buffer())
        };
        let resting = frame(&refresh_app(None));
        let editing = frame(&refresh_app(Some("60")));
        let field = resting
            .iter()
            .position(|r| r.contains("refresh") && !r.contains("refresh spent"))
            .unwrap_or_else(|| panic!("[{w}] refresh renders:\n{}", resting.join("\n")));
        let below = resting
            .iter()
            .position(|r| r.contains("refresh spent"))
            .unwrap_or_else(|| panic!("[{w}] refresh spent renders:\n{}", resting.join("\n")));
        assert!(
            editing[field + 1].contains("└ 10-3600 s"),
            "[{w}] the range tooltip sits under the edit line:\n{}",
            editing.join("\n")
        );
        crate::testutil::assert_rows_below_shift(
            &resting,
            &editing,
            field,
            below,
            delta,
            4,
            &w.to_string(),
        );
    }
}

/// S10: at 28 columns the typed refresh buffer keeps room for its digits:
/// the key column narrows so `300` draws whole beside its unit, the caret
/// right after it.
#[test]
fn a_typed_refresh_buffer_keeps_its_digits_visible_at_28_cols() {
    use crate::testutil::{draw_caret, symbol_at};
    let _home = crate::testutil::HomeSandbox::new();
    let app = refresh_app(Some("300"));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(28, 100)).unwrap();
    let (end, buf) = draw_caret(&mut term, |f| crate::tui::render::draw(f, &app));
    let row: String = crate::testutil::buffer_rows(&buf)[end.y as usize]
        .chars()
        .skip(2)
        .take(24)
        .collect();
    assert_eq!(
        row, "✎ refresh         300 s ",
        "the field row's content cells"
    );
    assert_eq!(end.x, 23, "the caret sits right after `300`");
    assert_eq!(symbol_at(&buf, end.x - 1, end.y), "0");
}

/// S10: a typed Config-tab row keeps its caret inside the pane on a narrow
/// terminal: the key column clamps and the label truncates (`…`), the Setup
/// clamp's shape, so `300` and its unit stay whole.
#[test]
fn a_typed_config_row_keeps_its_caret_inside_on_a_narrow_pane() {
    use crate::testutil::{draw_caret, right_border, symbol_at};
    use crate::tui::app::GlobalConfigRow;
    let _home = crate::testutil::HomeSandbox::new();
    for row in [
        GlobalConfigRow::RefreshInterval,
        GlobalConfigRow::WeeklyThreshold,
        GlobalConfigRow::ContextNudge,
    ] {
        for w in [16u16, 18, 20, 22, 28] {
            let app = config_typed_app(row, "300");
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, 100)).unwrap();
            let (end, buf) = draw_caret(&mut term, |f| crate::tui::render::draw(f, &app));
            let border = right_border(&buf, end.y);
            assert!(
                end.x + 2 <= border,
                "[{row:?} {w}] End stays inside the field ({end:?}, border {border})"
            );
            assert_eq!(
                symbol_at(&buf, end.x - 1, end.y),
                "0",
                "[{row:?} {w}] End follows `300`"
            );
        }
    }
}

#[test]
fn config_toggles_paint_their_parts_in_split_colours() {
    for tier in [
        crate::tui::theme::Tier::Full,
        crate::tui::theme::Tier::Compatible,
    ] {
        let _tier = crate::testutil::TierSandbox::new(tier);
        let mut off_state = toggles();
        off_state.auto_update = false;
        crate::testutil::assert_split_toggle(
            &one_row(
                GlobalConfigRow::AutoUpdate,
                false,
                toggles(),
                tunables(),
                None,
            ),
            true,
        );
        crate::testutil::assert_split_toggle(
            &one_row(
                GlobalConfigRow::AutoUpdate,
                false,
                off_state,
                tunables(),
                None,
            ),
            false,
        );
    }
}

/// The MCP login mode sits right after `non-login keys`, the other row about
/// what travels beside a login, in the scheduler band.
#[test]
fn sync_mcp_logins_sits_after_non_login_keys() {
    assert_eq!(GlobalConfigRow::SyncMcpLogins.band(), "scheduler");
    let pos = |row: GlobalConfigRow| {
        GLOBAL_CONFIG_ROWS
            .iter()
            .position(|r| *r == row)
            .expect("row in the config list")
    };
    assert_eq!(
        pos(GlobalConfigRow::SyncMcpLogins),
        pos(GlobalConfigRow::PreserveNonLoginKeys) + 1
    );
}

/// A plain toggle labelled `sync MCP logins`, with the exact hint per mode.
#[test]
fn sync_mcp_logins_renders_the_toggle_and_exact_hints() {
    let mut off_state = toggles();
    off_state.sync_mcp_logins = false;
    {
        let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);
        let on = line_text(&one_row(
            GlobalConfigRow::SyncMcpLogins,
            false,
            toggles(),
            tunables(),
            None,
        ));
        assert!(on.contains("sync MCP logins"), "the key reads: {on}");
        let off = line_text(&one_row(
            GlobalConfigRow::SyncMcpLogins,
            false,
            off_state,
            tunables(),
            None,
        ));
        assert!(off.contains("[off]"), "off renders the bracket: {off}");
    }
    assert_eq!(
        row_hint(GlobalConfigRow::SyncMcpLogins, toggles(), tunables()).expect("hint"),
        sync_mcp_logins_on_hint(cfg!(target_os = "macos"))
    );
    assert_eq!(
        sync_mcp_logins_on_hint(true),
        "every account and live session gets the newest login to each MCP server"
    );
    assert_eq!(
        sync_mcp_logins_on_hint(false),
        "every account gets the newest login to each MCP server, a running session at its next switch"
    );
    assert_eq!(
        row_hint(GlobalConfigRow::SyncMcpLogins, off_state, tunables()).expect("hint"),
        "each account keeps its own MCP server logins"
    );
}
