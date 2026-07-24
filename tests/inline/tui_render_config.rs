//! The Setup-tab `model` row is a segmented alias cycle sharing the Config-tab
//! contract: bare labels when blurred, the active option bracketed only on focus
//! (the row widens by 2 on focus — the bracket pair is the only width change).
//! Plus the pane's action rows, which take the `+ new`-row focus promotion.

use super::*;
use ratatui::style::Modifier;

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

// Blurred: bare labels, no brackets anywhere. Focused: the active preset wraps
// in `[]` and that bracket pair is the only width change (the old shape padded
// the blurred active option to ` label `, so blurred==focused in width — this
// delta is the contract).
#[test]
fn model_cycle_brackets_the_active_option_only_on_focus() {
    let arrow = Span::raw("  ");
    let blurred = line_text(&model_cycle_line(arrow.clone(), "sonnet", false));
    let focused = line_text(&model_cycle_line(arrow, "sonnet", true));

    assert!(
        blurred.contains("sonnet"),
        "active preset renders when blurred: {blurred}"
    );
    assert!(!blurred.contains('['), "blurred stays bare: {blurred}");
    assert!(
        focused.contains("[sonnet]"),
        "focused brackets the active preset: {focused}"
    );
    assert_eq!(
        focused.chars().count(),
        blurred.chars().count() + 2,
        "the bracket pair is the only width change on focus"
    );
}

// A custom id (no preset match) appends in ACCENT instead of mis-bracketing the
// nearest alias — and stays bracket-free when blurred.
#[test]
fn model_cycle_appends_a_custom_id_without_brackets() {
    let arrow = Span::raw("  ");
    let blurred = line_text(&model_cycle_line(arrow.clone(), "claude-fable-5", false));
    let focused = line_text(&model_cycle_line(arrow, "claude-fable-5", true));

    assert!(
        blurred.contains("claude-fable-5"),
        "custom id renders: {blurred}"
    );
    assert!(
        !blurred.contains('['),
        "no brackets on a custom id when blurred: {blurred}"
    );
    assert!(
        !focused.contains("[claude-fable-5]"),
        "a custom id is appended, not bracketed: {focused}"
    );
}

// The edit-mode `✎` glyph pairs `ACCENT + bold`, matching the cloudy-tui
// canonical pairing shared with the selection caret `❯` — this card rendered
// it accent-only (class bug, fixed at all four edit-glyph render sites).
#[test]
fn edit_glyph_is_bold_like_the_selection_caret() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let snap = Snap::blank("acct");
    let input = InputState::new("x");
    let editing = detail_row(ConfigRow::Name, true, true, None, &snap, &input);
    let glyph = &editing.spans[0];
    assert!(
        glyph.style.add_modifier.contains(Modifier::BOLD),
        "edit glyph must be bold: {glyph:?}"
    );
    assert_eq!(
        glyph.style.fg,
        theme::accent().fg,
        "edit glyph stays accent"
    );
}

// The selection caret pairs ACCENT + bold in every other card (chain.rs,
// overview.rs, panes.rs) — this card rendered it accent-only.
#[test]
fn selection_caret_is_bold_like_every_other_card() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let snap = Snap::blank("acct");
    let input = InputState::new("x");
    let line = detail_row(ConfigRow::Name, true, false, None, &snap, &input);
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

/// The pane's color-identity action rows take the `+ new`-row promotion: bold
/// when the cursor is on it, and the accent (or success) color held throughout,
/// since the color is the row's identity and never promotes to `TEXT`. Before
/// this they were bare labels that looked identical focused and blurred, leaving
/// only the row tint to carry focus. `delete account` / `log out` are NOT in
/// this set: their always-bold `DANGER` is a fixed destructive cue that must
/// persist whether or not the row is focused.
#[test]
fn action_rows_bold_on_select_and_keep_their_color() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut snap = Snap::blank("+ new account");
    let input = InputState::new("");
    // (row, the style the label holds in both states)
    let cases: [(ConfigRow, Style); 4] = [
        (ConfigRow::EnvAdd, theme::accent()),
        (ConfigRow::ModelOverrideAdd, theme::accent()),
        (ConfigRow::Create, theme::accent()),
        (ConfigRow::Login, theme::accent()),
    ];
    for (row, want) in cases {
        let blurred = detail_row(row, false, false, None, &snap, &input);
        let focused = detail_row(row, true, false, None, &snap, &input);
        let (b, f) = (&blurred.spans[1], &focused.spans[1]);
        assert_eq!(
            b.content, f.content,
            "{row:?}: focus must not change the label text"
        );
        assert!(
            !b.style.add_modifier.contains(Modifier::BOLD),
            "{row:?}: a blurred action row must not be bold"
        );
        assert!(
            f.style.add_modifier.contains(Modifier::BOLD),
            "{row:?}: a selected action row promotes to bold"
        );
        assert_eq!(b.style.fg, want.fg, "{row:?}: blurred keeps its color");
        assert_eq!(
            f.style.fg, want.fg,
            "{row:?}: the color is the row's identity, so focus never recolors it"
        );
    }

    // The `✓ logged in` state is the same row in SUCCESS — same promotion rule.
    snap.captured = true;
    let blurred = detail_row(ConfigRow::Login, false, false, None, &snap, &input);
    let focused = detail_row(ConfigRow::Login, true, false, None, &snap, &input);
    assert!(line_text(&focused).contains("✓ logged in"));
    assert!(!blurred.spans[1].style.add_modifier.contains(Modifier::BOLD));
    assert!(focused.spans[1].style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(focused.spans[1].style.fg, theme::success().fg);
}

/// Setup hints follow their row's current value — auto-start flips on/off, the
/// base-url hint flips between the claude.ai and custom-endpoint phrasings.
#[test]
fn setup_hints_follow_the_row_value() {
    let mut snap = Snap::blank("a");

    snap.auto_start = true;
    let on = row_hint(ConfigRow::AutoStart, &snap).unwrap();
    assert!(on.contains("throwaway session"), "{on}");
    snap.auto_start = false;
    let off = row_hint(ConfigRow::AutoStart, &snap).unwrap();
    assert!(off.contains("never starts"), "{off}");

    snap.base_url = String::new();
    let empty = row_hint(ConfigRow::BaseUrl, &snap).unwrap();
    assert!(empty.contains("claude.ai account"), "{empty}");
    snap.base_url = "https://api.example.com".into();
    let set = row_hint(ConfigRow::BaseUrl, &snap).unwrap();
    assert!(set.contains("calls instead"), "{set}");
}

// ── `disabled` row (account-action button, same class as `Delete`) ─────────

/// Disabling is the real-impact direction: it renders in the exact same
/// button class as `Delete` — a single label span, DANGER + bold
/// unconditionally (not just when focused), and the label flips to the
/// "press again" copy once `armed_action` names this row. Mirrors
/// `ConfigRow::Delete`'s own rendering one-for-one so a reviewer can diff the
/// two match arms directly.
#[test]
fn disable_button_is_delete_class_danger_and_arms_on_second_press() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let snap = Snap::blank("a"); // disabled: false → the disable direction
    let input = InputState::new("");

    for selected in [false, true] {
        let arrow = if selected { "❯ " } else { "  " };
        let unarmed = detail_row(ConfigRow::Disabled, selected, false, None, &snap, &input);
        assert_eq!(
            line_text(&unarmed),
            format!("{arrow}disable account"),
            "unarmed label reads 'disable account' regardless of focus (selected={selected})"
        );
        assert_eq!(
            unarmed.spans[1].style.fg,
            theme::danger().fg,
            "unarmed disable renders DANGER"
        );
        assert!(
            unarmed.spans[1].style.add_modifier.contains(Modifier::BOLD),
            "disable is always bold, unlike the accent bold-on-select class (selected={selected})"
        );

        let armed = detail_row(
            ConfigRow::Disabled,
            selected,
            false,
            Some(ConfigRow::Disabled),
            &snap,
            &input,
        );
        assert_eq!(
            line_text(&armed),
            format!("{arrow}press again to disable"),
            "arming (this row named in armed_action) swaps to the confirm copy"
        );
        assert_eq!(armed.spans[1].style.fg, theme::danger().fg);
        assert!(armed.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    // `armed_action` naming a DIFFERENT row (e.g. `Delete`) must not bleed
    // into this row's confirm copy — only its own row name arms it.
    let cross_armed = detail_row(
        ConfigRow::Disabled,
        true,
        false,
        Some(ConfigRow::Delete),
        &snap,
        &input,
    );
    assert_eq!(line_text(&cross_armed), "❯ disable account");
}

/// Enabling is harmless — it takes the accent, bold-only-when-selected class
/// shared with `Login`/`Create`/`+ add env` instead of Delete's always-bold
/// DANGER, and it never shows a "press again" confirm copy (immediate, never
/// armed).
#[test]
fn enable_button_is_accent_class_bold_only_on_select() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut snap = Snap::blank("a");
    snap.disabled = true; // currently disabled → the enable direction
    let input = InputState::new("");

    let blurred = detail_row(ConfigRow::Disabled, false, false, None, &snap, &input);
    let focused = detail_row(ConfigRow::Disabled, true, false, None, &snap, &input);
    assert_eq!(line_text(&blurred), "  enable account");
    assert_eq!(line_text(&focused), "❯ enable account");
    assert_eq!(blurred.spans[1].style.fg, theme::accent().fg);
    assert_eq!(focused.spans[1].style.fg, theme::accent().fg);
    assert!(
        !blurred.spans[1].style.add_modifier.contains(Modifier::BOLD),
        "blurred enable is not bold"
    );
    assert!(
        focused.spans[1].style.add_modifier.contains(Modifier::BOLD),
        "selected enable promotes to bold"
    );

    // An armed_action left over from the disable direction must not surface
    // a "press again" copy once the account is actually disabled — enabling
    // never arms, so it has nothing to confirm.
    let stale_armed = detail_row(
        ConfigRow::Disabled,
        true,
        false,
        Some(ConfigRow::Disabled),
        &snap,
        &input,
    );
    assert_eq!(line_text(&stale_armed), "❯ enable account");
}

/// Dimmed/inert while gated (active account or a live session), matching the
/// Fallback tab's `max spend` treatment: the whole row — arrow and label —
/// renders faint, the label falls back to the plain (non-armed) copy even if
/// `armed_action` names this row, and the gate wins over both directions.
/// The differential half: a gated row's color must not match a normal,
/// ungated action row's (`Login`) — proving the dim is a real style change,
/// not just a coincidental faint that also happens to be accent/danger.
#[test]
fn disable_button_dims_while_gated_and_ignores_a_stale_arm() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut snap = Snap::blank("a");
    let input = InputState::new("");

    snap.is_active = true;
    let gated_active = detail_row(
        ConfigRow::Disabled,
        true,
        false,
        Some(ConfigRow::Disabled),
        &snap,
        &input,
    );
    assert_eq!(
        line_text(&gated_active),
        "❯ disable account",
        "gated ignores the stale arm, showing the plain label"
    );
    assert_eq!(gated_active.spans[1].style.fg, theme::faint().fg);
    assert_eq!(
        gated_active.spans[0].style.fg,
        theme::faint().fg,
        "the arrow dims too while gated and selected"
    );

    snap.is_active = false;
    snap.has_live_session = true;
    let gated_session = detail_row(ConfigRow::Disabled, true, false, None, &snap, &input);
    assert_eq!(gated_session.spans[1].style.fg, theme::faint().fg);

    // Gated while already disabled reads "enable account", still faint.
    snap.disabled = true;
    let gated_enable = detail_row(ConfigRow::Disabled, true, false, None, &snap, &input);
    assert_eq!(line_text(&gated_enable), "❯ enable account");
    assert_eq!(gated_enable.spans[1].style.fg, theme::faint().fg);

    // Differential: the gated row's color must differ from a normal,
    // ungated action row's — proves the dim is a real style branch, not an
    // accident of the two colors overlapping.
    let normal_snap = Snap::blank("a");
    let normal_action = detail_row(ConfigRow::Login, true, false, None, &normal_snap, &input);
    assert_ne!(
        gated_session.spans[1].style.fg, normal_action.spans[1].style.fg,
        "a gated account-action row must render distinctly from a normal one"
    );
}

/// The `disabled` hint is value-aware: each gate names its own CLI-parity fix
/// first (checked ahead of the plain on/off state, since a gate can only ever
/// bite the not-yet-disabled state), then the on/off state describes what the
/// toggle does from here.
#[test]
fn disabled_hint_follows_the_gate_then_the_value() {
    let mut snap = Snap::blank("a");

    let off = row_hint(ConfigRow::Disabled, &snap).unwrap();
    assert!(off.contains("removes this account"), "{off}");

    snap.disabled = true;
    let on = row_hint(ConfigRow::Disabled, &snap).unwrap();
    assert!(on.contains("excluded from auto-switch"), "{on}");

    snap.disabled = false;
    snap.has_live_session = true;
    let session = row_hint(ConfigRow::Disabled, &snap).unwrap();
    assert!(session.contains("open session"), "{session}");

    // The active-account gate outranks the live-session gate.
    snap.is_active = true;
    let active = row_hint(ConfigRow::Disabled, &snap).unwrap();
    assert!(active.contains("active account"), "{active}");
}

/// A disabled account's row in the Setup account list carries the dim name and
/// nothing else — the label itself lives on the Setup header's own `status`
/// row, not on every list that happens to print the name. Driven through
/// `draw_selector` rather than `picker_row`, so it pins the CALL SITE's style
/// choice; handing `picker_row` a style would only re-assert the argument.
#[test]
fn disabled_account_only_dims_its_name_in_the_setup_list() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    use crate::profile::{AppConfig, AppState, ProfileName};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut disabled = crate::testutil::blank_profile("xqzoff");
    disabled.disabled = true;
    let enabled = crate::testutil::blank_profile("xqzon");
    let names: Vec<ProfileName> = vec!["xqzoff".into(), "xqzon".into()];
    let app = App::new(AppConfig {
        state: AppState {
            profiles: names,
            ..AppState::default()
        },
        profiles: vec![disabled, enabled],
    });

    let (w, h) = (40u16, 10u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw_selector(f, f.area(), &app, true))
        .unwrap();
    let buf = term.backend().buffer();
    let rows = crate::testutil::buffer_rows(buf);

    let cell_fg = |needle: &str| -> Option<ratatui::style::Color> {
        let row_idx = rows
            .iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("{needle} renders:\n{}", rows.join("\n")));
        let byte = rows[row_idx].find(needle).unwrap();
        let col = rows[row_idx][..byte].chars().count();
        Some(buf.content[row_idx * w as usize + col].fg)
    };

    assert_eq!(
        cell_fg("xqzoff"),
        theme::dim().fg,
        "the disabled account's name renders dim"
    );
    assert_ne!(
        cell_fg("xqzon"),
        theme::dim().fg,
        "an enabled sibling keeps its ordinary name color"
    );
    assert!(
        !rows.iter().any(|r| r.contains("disabled")),
        "no inline chip anywhere in the list:\n{}",
        rows.join("\n")
    );
}

/// The Setup header's `status` row: the one Setup-tab surface naming the
/// disabled state, sitting ABOVE `type`. Status purity — an enabled account
/// renders no row at all rather than a `[ enabled ]` non-status.
#[test]
fn setup_status_row_renders_only_while_disabled() {
    use crate::profile::{AppConfig, AppState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let rows_for = |disabled: bool| -> Vec<String> {
        let mut snap = Snap::blank("acct");
        snap.disabled = disabled;
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let app = App::new(AppConfig {
            state: AppState::default(),
            profiles: Vec::new(),
        });
        term.draw(|f| {
            draw_settings_rows(f, f.area(), &app, &[], 0, &snap, false);
        })
        .unwrap();
        crate::testutil::buffer_rows(term.backend().buffer())
    };

    let off = rows_for(false);
    assert!(
        !off.iter().any(|r| r.contains("status")),
        "an enabled account has no status row:\n{}",
        off.join("\n")
    );

    let on = rows_for(true);
    let status_idx = on
        .iter()
        .position(|r| r.contains("status"))
        .unwrap_or_else(|| panic!("status row renders while disabled:\n{}", on.join("\n")));
    assert!(
        on[status_idx].contains("[ disabled ]"),
        "the status value is the shared pill: {}",
        on[status_idx]
    );
    let type_idx = on
        .iter()
        .position(|r| r.contains("type"))
        .expect("type row renders");
    assert!(
        status_idx < type_idx,
        "status sits ABOVE type (status at {status_idx}, type at {type_idx})"
    );
}

// ── CLA-SPLIT: the `token` long-lived-login status row ──────────────────────

// A comfortable horizon is a plain accent value; the last 30 days warn as a
// pill; expired and mis-filled escalate to a DANGER pill plus a `└` fix line
// (the operator thinks the split is armed and it isn't). Unstamped says so.
#[test]
fn long_lived_token_row_counts_down_and_escalates() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    use crate::claude::SessionTokenStatus as S;
    let day = 86_400_000_i64;
    let now = 1_700_000_000_000_i64;
    let w = 60usize;
    let text = |ls: &[Line<'static>]| -> String {
        ls.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect()
    };
    // Match on fg only, so the pill label's added BOLD doesn't defeat the check.
    let has_color = |ls: &[Line<'static>], st: Style| {
        ls.iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.style.fg == st.fg)
    };

    // Comfortable horizon: a plain accent value, no pill, one line.
    let comfy = session_token_lines(&S::LongLived(Some(now + 340 * day)), false, now, w);
    assert_eq!(comfy.len(), 1);
    let comfy_t = text(&comfy);
    assert!(comfy_t.contains("token"), "{comfy_t}");
    assert!(comfy_t.contains("long-lived · ~340d left"), "{comfy_t}");
    assert!(
        !comfy_t.contains('['),
        "comfortable is a value, not a pill: {comfy_t}"
    );
    assert!(has_color(&comfy, theme::accent()));

    // Last 30 days: a WARNING pill, still one line, no fix.
    let soon = session_token_lines(&S::LongLived(Some(now + 12 * day)), false, now, w);
    assert_eq!(soon.len(), 1);
    assert!(
        text(&soon).contains("[ expires in ~12d ]"),
        "{}",
        text(&soon)
    );
    assert!(has_color(&soon, theme::warning()), "last 30 days warn");

    // Expired: DANGER pill + a `└` re-mint fix line.
    let dead = session_token_lines(&S::LongLived(Some(now - day)), false, now, w);
    assert_eq!(dead.len(), 2, "expired = pill + fix line");
    let dead_t = text(&dead);
    assert!(dead_t.contains("[ expired ]"), "{dead_t}");
    assert!(
        dead_t.contains("re-mint with claude setup-token"),
        "{dead_t}"
    );
    assert!(has_color(&dead, theme::danger()), "expired is DANGER");

    // Expired within the last 24h: truncating division gives 0 days; it must
    // read as expired, not "~0d / warning".
    let just_dead = session_token_lines(&S::LongLived(Some(now - day / 2)), false, now, w);
    let just_dead_t = text(&just_dead);
    assert!(
        just_dead_t.contains("[ expired ]"),
        "a token expired <24h ago is expired, not ~0d: {just_dead_t}"
    );
    assert!(
        has_color(&just_dead, theme::danger()),
        "sub-day-expired is DANGER"
    );

    // Unstamped long-lived: a plain accent value.
    let unstamped = session_token_lines(&S::LongLived(None), false, now, w);
    assert_eq!(unstamped.len(), 1);
    assert!(
        text(&unstamped).contains("no recorded expiry"),
        "{}",
        text(&unstamped)
    );

    // Mis-filled (rotating pair): DANGER pill + fix, split disengaged.
    let misfilled = session_token_lines(&S::NotLongLived, false, now, w);
    assert_eq!(misfilled.len(), 2, "mis-filled = pill + fix line");
    let mis_t = text(&misfilled);
    assert!(mis_t.contains("[ mis-filled ]"), "{mis_t}");
    assert!(
        mis_t.contains("sidecar has a refresh token, split is off"),
        "{mis_t}"
    );
    assert!(
        has_color(&misfilled, theme::danger()),
        "a disengaged sidecar is DANGER"
    );
}
