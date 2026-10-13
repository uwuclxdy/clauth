#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Setup-tab `model` row is a segmented alias cycle sharing the Config-tab
//! contract: bare labels when blurred, the active option bracketed only on focus
//! (the row widens by 2 on focus — the bracket pair is the only width change).
//! Plus the pane's action rows, which take the `+ new`-row focus promotion.

use super::*;
use ratatui::style::Modifier;

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Wide enough that no row wraps: these pins are about one line's geometry.
const WIDE: usize = 200;

/// [`detail_row`] in a pane wide enough that the row is one line.
fn one_row(
    row: ConfigRow,
    selected: bool,
    editing: bool,
    armed_action: Option<ConfigRow>,
    snap: &Snap,
    input: &InputState,
) -> Line<'static> {
    let mut lines = detail_row(row, selected, editing, armed_action, snap, input, WIDE);
    assert_eq!(lines.len(), 1, "a wide pane holds the row on one line");
    lines.remove(0)
}

/// [`model_cycle_lines`] in a pane wide enough that the row is one line.
fn model_line(arrow: Span<'static>, current: &str, selected: bool) -> Line<'static> {
    let mut lines = model_cycle_lines(arrow, current, selected, WIDE);
    assert_eq!(lines.len(), 1, "a wide pane holds the row on one line");
    lines.remove(0)
}

// Blurred: bare labels, no brackets anywhere. Focused: the active preset wraps
// in `[]` and that bracket pair is the only width change (the old shape padded
// the blurred active option to ` label `, so blurred==focused in width — this
// delta is the contract).
#[test]
fn model_cycle_brackets_the_active_option_only_on_focus() {
    let arrow = Span::raw("  ");
    let blurred = line_text(&model_line(arrow.clone(), "sonnet", false));
    let focused = line_text(&model_line(arrow, "sonnet", true));

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

#[test]
fn model_cycle_lists_fable_first_after_default() {
    let line = line_text(&model_line(Span::raw("  "), "", false));
    let options = line
        .trim_start()
        .strip_prefix("model")
        .expect("the row opens on its key");
    assert_eq!(
        options.split_whitespace().collect::<Vec<_>>(),
        ["default", "fable", "opus", "sonnet", "haiku", "opusplan"]
    );
}

// A custom id (no preset match) appends in ACCENT instead of mis-bracketing the
// nearest alias — and stays bracket-free when blurred.
#[test]
fn model_cycle_appends_a_custom_id_without_brackets() {
    let arrow = Span::raw("  ");
    let blurred = line_text(&model_line(arrow.clone(), "claude-fable-5", false));
    let focused = line_text(&model_line(arrow, "claude-fable-5", true));

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

// The edit-mode `✎` glyph pairs `ACCENT + bold`, matching the canonical
// pairing shared with the selection caret `❯` — this card rendered
// it accent-only (class bug, fixed at all four edit-glyph render sites).
#[test]
fn edit_glyph_is_bold_like_the_selection_caret() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let snap = Snap::blank("acct");
    let input = InputState::new("x");
    let editing = one_row(ConfigRow::Name, true, true, None, &snap, &input);
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
    let line = one_row(ConfigRow::Name, true, false, None, &snap, &input);
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
        let blurred = one_row(row, false, false, None, &snap, &input);
        let focused = one_row(row, true, false, None, &snap, &input);
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
    let blurred = one_row(ConfigRow::Login, false, false, None, &snap, &input);
    let focused = one_row(ConfigRow::Login, true, false, None, &snap, &input);
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
    assert_eq!(
        on,
        "sends a 1-token request to Haiku at every 5h usage reset"
    );
    snap.auto_start = false;
    let off = row_hint(ConfigRow::AutoStart, &snap).unwrap();
    assert_eq!(off, "5h usage window stays closed after reset");

    snap.base_url = String::new();
    let empty = row_hint(ConfigRow::BaseUrl, &snap).unwrap();
    assert_eq!(
        empty,
        "leave empty for a Claude account, or set an API endpoint"
    );
    snap.base_url = "https://api.example.com".into();
    let set = row_hint(ConfigRow::BaseUrl, &snap).unwrap();
    assert_eq!(
        set,
        "the API endpoint this account calls instead of claude.ai"
    );
}

/// Field rows whose labels self-describe carry no hint (a hint restating the
/// label reads as noise); the siblings that keep one are pinned whole.
#[test]
fn self_describing_rows_have_no_hint_and_sibling_hints_are_pinned() {
    let mut snap = Snap::blank("a");
    for row in [
        ConfigRow::Model,
        ConfigRow::OpusModel,
        ConfigRow::SonnetModel,
        ConfigRow::HaikuModel,
        ConfigRow::FableModel,
        ConfigRow::EnvEntry(0),
        ConfigRow::EnvAdd,
    ] {
        assert_eq!(row_hint(row, &snap), None, "{row:?} carries no hint");
    }
    assert_eq!(
        row_hint(ConfigRow::SubagentModel, &snap).as_deref(),
        Some("default subagent model in this account"),
    );
    assert_eq!(
        row_hint(ConfigRow::ModelOverrideAdd, &snap).as_deref(),
        Some("set custom model names on this account"),
    );
    assert_eq!(
        row_hint(ConfigRow::ApiKey, &snap).as_deref(),
        Some("provided to Claude Code via \"apiKeyHelper\" field"),
    );
    snap.console_login = true;
    assert_eq!(
        row_hint(ConfigRow::Login, &snap).as_deref(),
        Some("log into alibaba console to see this account's usage"),
    );
}

/// The capture + log-out hints state the act alone, nothing else.
#[test]
fn capture_and_logout_hints_state_the_act_alone() {
    let mut snap = Snap::blank("a");
    assert_eq!(
        row_hint(ConfigRow::CaptureLogin, &snap).as_deref(),
        Some("save the current global credentials into a new account"),
    );
    assert_eq!(
        row_hint(ConfigRow::DeleteCreds, &snap).as_deref(),
        Some("clears the stored OAuth login"),
    );
    // `api_login` flips on `!login_is_oauth`, mirroring row_hint's own guard.
    snap.login_is_oauth = false;
    assert_eq!(
        row_hint(ConfigRow::DeleteCreds, &snap).as_deref(),
        Some("clears the stored API key"),
    );
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
        let unarmed = one_row(ConfigRow::Disabled, selected, false, None, &snap, &input);
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

        let armed = one_row(
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
    let cross_armed = one_row(
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

    let blurred = one_row(ConfigRow::Disabled, false, false, None, &snap, &input);
    let focused = one_row(ConfigRow::Disabled, true, false, None, &snap, &input);
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
    let stale_armed = one_row(
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
    let gated_active = one_row(
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
    let gated_session = one_row(ConfigRow::Disabled, true, false, None, &snap, &input);
    assert_eq!(gated_session.spans[1].style.fg, theme::faint().fg);

    // Gated while already disabled reads "enable account", still faint.
    snap.disabled = true;
    let gated_enable = one_row(ConfigRow::Disabled, true, false, None, &snap, &input);
    assert_eq!(line_text(&gated_enable), "❯ enable account");
    assert_eq!(gated_enable.spans[1].style.fg, theme::faint().fg);

    // Differential: the gated row's color must differ from a normal,
    // ungated action row's — proves the dim is a real style branch, not an
    // accident of the two colors overlapping.
    let normal_snap = Snap::blank("a");
    let normal_action = one_row(ConfigRow::Login, true, false, None, &normal_snap, &input);
    assert_ne!(
        gated_session.spans[1].style.fg, normal_action.spans[1].style.fg,
        "a gated account-action row must render distinctly from a normal one"
    );
}

/// The `disabled` hint is value-aware: each gate names its own CLI-parity fix
/// first (checked ahead of the plain on/off state, since a gate can only ever
/// bite the not-yet-disabled state), then the on/off state describes what the
/// toggle does from here.
///
/// Every arm is pinned WHOLE, not by a fragment. A `.contains` here let the
/// active-account gate carry an em-dash — the one separator shipped prose bans,
/// already swept out of it here — and stay green through the fix and past a
/// revert of it. The house separator (`·` or a comma) and the app-wide
/// `live session` noun both live in these four strings.
#[test]
fn disabled_hint_follows_the_gate_then_the_value() {
    let mut snap = Snap::blank("a");

    assert_eq!(
        row_hint(ConfigRow::Disabled, &snap).as_deref(),
        Some("excludes this account from auto-switch, usage polling, and status until re-enabled"),
    );

    snap.disabled = true;
    assert_eq!(
        row_hint(ConfigRow::Disabled, &snap).as_deref(),
        Some("excluded from auto-switch, usage polling, and status until re-enabled"),
    );

    snap.disabled = false;
    snap.has_live_session = true;
    assert_eq!(
        row_hint(ConfigRow::Disabled, &snap).as_deref(),
        Some("cannot disable an account with a live session"),
    );

    // The active-account gate outranks the live-session gate.
    snap.is_active = true;
    assert_eq!(
        row_hint(ConfigRow::Disabled, &snap).as_deref(),
        Some("cannot disable the global active account"),
    );
}

/// A disabled account's row in the Setup account list carries the dim name and
/// nothing else — the label itself lives on the Setup header's own `status`
/// row, not on every list that happens to print the name. Driven through
/// `draw_selector` rather than `picker_row`, so it pins the CALL SITE's style
/// choice; handing `picker_row` a style would only re-assert the argument.
#[test]
fn disabled_account_only_dims_its_name_in_the_setup_list() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    use crate::profile::{AppConfig, AppState, ProfileName};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut disabled = crate::testutil::blank_profile(&crate::profile::ProfileName::from("xqzoff"));
    disabled.disabled = true;
    let enabled = crate::testutil::blank_profile(&crate::profile::ProfileName::from("xqzon"));
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
    let _home = crate::testutil::HomeSandbox::new();
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

/// Render the Setup header for a blank snapshot carrying `email`, `base_url`
/// and a long-lived `token` state, at `width` cells.
fn email_header(
    email: Option<&str>,
    base_url: &str,
    token: Option<crate::claude::SessionTokenStatus>,
    width: u16,
) -> (Vec<String>, ratatui::buffer::Buffer) {
    use crate::profile::{AppConfig, AppState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut snap = Snap::blank("acct");
    snap.email = email.map(str::to_string);
    snap.base_url = base_url.to_string();
    snap.session_token = token;
    let app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    let mut term = Terminal::new(TestBackend::new(width, 20)).unwrap();
    term.draw(|f| draw_settings_rows(f, f.area(), &app, &[], 0, &snap, false))
        .unwrap();
    let buf = term.backend().buffer().clone();
    (crate::testutil::buffer_rows(&buf), buf)
}

/// The Setup header's `email` line: the account's address right under `type`, the key in the static-label style and the value in
/// accent like `type`'s own, exactly once.
#[test]
fn setup_email_row_sits_under_type_in_accent() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (rows, buf) = email_header(Some("cloudy@example.com"), "", None, 60);
    assert_eq!(
        rows.iter()
            .filter(|r| r.contains("cloudy@example.com"))
            .count(),
        1,
        "the address renders exactly once:\n{}",
        rows.join("\n")
    );
    let type_idx = rows
        .iter()
        .position(|r| r.starts_with("type"))
        .unwrap_or_else(|| panic!("type row renders:\n{}", rows.join("\n")));
    assert_eq!(
        rows[type_idx + 1].trim_end(),
        "email           cloudy@example.com",
        "the email line sits right under type:\n{}",
        rows.join("\n")
    );
    let cell = |x: usize| &buf.content[(type_idx + 1) * 60 + x];
    assert_eq!(
        cell(0).style().fg,
        theme::label().fg,
        "the key takes the static label color"
    );
    assert!(
        cell(0).style().add_modifier.contains(Modifier::BOLD),
        "the key takes the static label bold"
    );
    assert_eq!(
        cell(16).style().fg,
        theme::accent().fg,
        "the address takes the accent value color"
    );
}

/// No address, no line: an account `/profile` never reported one for (or an
/// unfetched one), and an api-typed account, whose draft base url hides the
/// line with the `oauth` type it belongs to.
#[test]
fn setup_email_row_renders_only_for_a_known_oauth_address() {
    let _home = crate::testutil::HomeSandbox::new();
    let (rows, _) = email_header(None, "", None, 60);
    assert!(
        !rows.iter().any(|r| r.starts_with("email")),
        "no cached address renders no line:\n{}",
        rows.join("\n")
    );
    let (rows, _) = email_header(
        Some("cloudy@example.com"),
        "https://api.example.com",
        None,
        60,
    );
    assert!(
        !rows.iter().any(|r| r.contains("cloudy@example.com")),
        "an api-typed account renders no address:\n{}",
        rows.join("\n")
    );
}

/// An address wider than the pane keeps both ends around a middle ellipsis,
/// the shared truncation for identifiers, instead of clipping at the border:
/// 30 cells less the 16-cell key leave 14, so 7 head + `…` + 6 tail.
#[test]
fn a_long_setup_email_keeps_both_ends() {
    let _home = crate::testutil::HomeSandbox::new();
    let (rows, _) = email_header(Some("averyveryverylongname@example.com"), "", None, 30);
    assert!(
        rows.iter().any(|r| r == "email           averyve…le.com"),
        "the address middle-truncates to the pane:\n{}",
        rows.join("\n")
    );
}

/// On a long-lived-token account the address keeps its place under `type`,
/// with the `token` row after it, the order the ruling's preview shows.
#[test]
fn setup_email_row_sits_between_type_and_token() {
    let _home = crate::testutil::HomeSandbox::new();
    let (rows, _) = email_header(
        Some("cloudy@example.com"),
        "",
        Some(crate::claude::SessionTokenStatus::LongLived(None)),
        60,
    );
    let type_idx = rows
        .iter()
        .position(|r| r.starts_with("type"))
        .unwrap_or_else(|| panic!("type row renders:\n{}", rows.join("\n")));
    assert!(
        rows[type_idx + 1].starts_with("email ") && rows[type_idx + 2].starts_with("token "),
        "type, then email, then token:\n{}",
        rows.join("\n")
    );
}

/// A stored credentials file holding an OAuth pair.
fn oauth_login() -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// `build_snap`'s `email` for one account whose plan carries an address read
/// for account `u-1`, holding `credentials`, anchored to `anchor`, its
/// `/profile` stamped, then marked stale by a copy-back when `copied_back`.
fn snap_email(
    credentials: Option<crate::profile::ClaudeCredentials>,
    anchor: Option<&str>,
    copied_back: bool,
) -> Option<String> {
    use crate::profile::{AccountId, AppConfig, AppState, ProfileName};
    let _home = crate::testutil::HomeSandbox::new();
    let name = ProfileName::from("acct");
    let mut profile = crate::testutil::blank_profile(&name);
    profile.credentials = credentials;
    crate::profile::save_profile(&profile).expect("save the profile");
    crate::profile::save_app_state(&AppState {
        profiles: vec![name.clone()],
        ..AppState::default()
    })
    .expect("save the roster");
    if let Some(anchor) = anchor {
        crate::profile_cache::write_profile_cache(
            &name,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            &AccountId::from(anchor),
        );
    }
    crate::profile_cache::write_profile_cache(
        &name,
        crate::profile_cache::PROFILE_FETCHED_CACHE_FILE,
        &1_u64,
    );
    if copied_back {
        let store = crate::profile::profile_dir(&name)
            .expect("profile dir")
            .join(".credentials.json");
        crate::usage::mark_profile_read_stale(&store);
    }
    profile.usage = Some(crate::usage::UsageInfo {
        plan: Some(crate::usage::PlanInfo {
            email: Some(crate::usage::AccountEmail {
                address: "cloudy\u{1b}[2J@example.com".to_string(),
                account: AccountId::from("u-1"),
            }),
            ..crate::usage::PlanInfo::default()
        }),
        ..crate::usage::UsageInfo::default()
    });
    let app = App::new(AppConfig {
        state: AppState {
            profiles: vec![name],
            ..AppState::default()
        },
        profiles: vec![profile],
    });
    build_snap(&app, true).email
}

/// The login's own address reaches the snapshot, its control characters
/// escaped: it is upstream text bound for the terminal.
#[test]
fn snap_email_names_the_logins_own_address_escaped() {
    assert_eq!(
        snap_email(Some(oauth_login()), Some("u-1"), false).as_deref(),
        Some("cloudy\\u{1b}[2J@example.com")
    );
}

/// A plan carried over from an earlier login keeps that login's pair, and the
/// anchor names the login held now: the address names no one.
#[test]
fn snap_email_names_no_one_for_another_logins_address() {
    assert_eq!(snap_email(Some(oauth_login()), Some("u-2"), false), None);
}

/// No anchor, nothing to tie the address to.
#[test]
fn snap_email_names_no_one_without_an_anchor() {
    assert_eq!(snap_email(Some(oauth_login()), None, false), None);
}

/// A logged-out account names no one, whatever its cached plan says.
#[test]
fn snap_email_names_no_one_on_a_logged_out_account() {
    assert_eq!(snap_email(None, Some("u-1"), false), None);
}

/// A copy-back can install another account's login under the same anchor;
/// its stale mark hides the address until the next `/profile` read.
#[test]
fn snap_email_names_no_one_after_a_copy_back() {
    assert_eq!(snap_email(Some(oauth_login()), Some("u-1"), true), None);
}

/// A credentials file without an OAuth pair holds no login to name.
#[test]
fn snap_email_names_no_one_without_an_oauth_pair() {
    let empty = crate::profile::ClaudeCredentials {
        claude_ai_oauth: None,
    };
    assert_eq!(snap_email(Some(empty), Some("u-1"), false), None);
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
    let comfy = session_token_lines(&S::LongLived(Some(now + 340 * day)), false, "acct", now, w);
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
    let soon = session_token_lines(&S::LongLived(Some(now + 12 * day)), false, "acct", now, w);
    assert_eq!(soon.len(), 1);
    assert!(
        text(&soon).contains("[ expires in ~12d ]"),
        "{}",
        text(&soon)
    );
    assert!(has_color(&soon, theme::warning()), "last 30 days warn");

    // Expired: DANGER pill + a `└` re-mint fix line.
    let dead = session_token_lines(&S::LongLived(Some(now - day)), false, "acct", now, w);
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
    let just_dead = session_token_lines(&S::LongLived(Some(now - day / 2)), false, "acct", now, w);
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
    let unstamped = session_token_lines(&S::LongLived(None), false, "acct", now, w);
    assert_eq!(unstamped.len(), 1);
    assert!(
        text(&unstamped).contains("no recorded expiry"),
        "{}",
        text(&unstamped)
    );

    // Mis-filled (rotating pair): DANGER pill + fix, split disengaged.
    let misfilled = session_token_lines(&S::NotLongLived, false, "acct", now, w);
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

/// The action next to that status row. Same button class as `Delete` /
/// `Disabled`: one label span, DANGER + bold whether or not the row is focused,
/// flipping to the `press again to <verb>` copy once `armed_action` names it —
/// clearing changes what EVERY future switch installs and can move a running
/// session's credentials, so it is never a one-press action.
#[test]
fn clear_session_token_button_is_delete_class_and_arms_on_second_press() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut snap = Snap::blank("a");
    snap.has_other_login = true; // ungated: the profile has an OAuth login too
    let input = InputState::new("");

    for selected in [false, true] {
        let arrow = if selected { "❯ " } else { "  " };
        let unarmed = one_row(
            ConfigRow::ClearSessionToken,
            selected,
            false,
            None,
            &snap,
            &input,
        );
        assert_eq!(
            line_text(&unarmed),
            format!("{arrow}clear long-lived token"),
            "unarmed label is fixed regardless of focus (selected={selected})"
        );
        assert_eq!(unarmed.spans[1].style.fg, theme::danger().fg);
        assert!(
            unarmed.spans[1].style.add_modifier.contains(Modifier::BOLD),
            "always bold, like `delete account` (selected={selected})"
        );

        let armed = one_row(
            ConfigRow::ClearSessionToken,
            selected,
            false,
            Some(ConfigRow::ClearSessionToken),
            &snap,
            &input,
        );
        assert_eq!(
            line_text(&armed),
            format!("{arrow}press again to clear"),
            "arming swaps to the confirm copy"
        );
        assert_eq!(armed.spans[1].style.fg, theme::danger().fg);
        assert!(armed.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    // Another row's arm must not bleed into this row's confirm copy.
    let cross_armed = one_row(
        ConfigRow::ClearSessionToken,
        true,
        false,
        Some(ConfigRow::Delete),
        &snap,
        &input,
    );
    assert_eq!(line_text(&cross_armed), "❯ clear long-lived token");
}

/// Dimmed + inert when the profile stores no other login — clearing there
/// would strip its only credential, so the row takes the same faint treatment
/// as a gated `disabled` row (arrow included) and ignores a stale arm. The
/// differential leg proves the faint is a real style branch rather than a
/// coincidence with an ungated row's color.
#[test]
fn clear_session_token_button_dims_without_another_stored_login() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let snap = Snap::blank("a"); // has_other_login: false
    let input = InputState::new("");

    let gated = one_row(
        ConfigRow::ClearSessionToken,
        true,
        false,
        Some(ConfigRow::ClearSessionToken),
        &snap,
        &input,
    );
    assert_eq!(
        line_text(&gated),
        "❯ clear long-lived token",
        "gated ignores the stale arm, showing the plain label"
    );
    assert_eq!(gated.spans[1].style.fg, theme::faint().fg);
    assert_eq!(
        gated.spans[0].style.fg,
        theme::faint().fg,
        "the arrow dims too while gated and selected"
    );

    let mut ungated_snap = Snap::blank("a");
    ungated_snap.has_other_login = true;
    let ungated = one_row(
        ConfigRow::ClearSessionToken,
        true,
        false,
        None,
        &ungated_snap,
        &input,
    );
    assert_ne!(
        gated.spans[1].style.fg, ungated.spans[1].style.fg,
        "a gated row must render distinctly from the same row ungated"
    );

    // The FLAG-ONLY state is never gated, even with no other login: the press
    // disarms without touching a credential (`run_config_row`'s widened gate),
    // and a row that dims while its press acts would be the renderer's own
    // lie. `Snap::clear_gated` is the one spelling all three surfaces share.
    let mut flag_only = Snap::blank("a");
    flag_only.rolling_armed = true;
    let acting = one_row(
        ConfigRow::ClearSessionToken,
        true,
        false,
        None,
        &flag_only,
        &input,
    );
    assert_eq!(
        acting.spans[1].style.fg,
        theme::danger().fg,
        "a flag-only account renders the acting button, not the dim"
    );
    let armed = one_row(
        ConfigRow::ClearSessionToken,
        true,
        false,
        Some(ConfigRow::ClearSessionToken),
        &flag_only,
        &input,
    );
    assert_eq!(
        line_text(&armed),
        "❯ press again to clear",
        "the first press arms VISIBLY on a flag-only account"
    );
}

/// The `└` hint is value-aware like every other Setup hint: the gate's reason
/// first, then what the clear actually does — and the ACTIVE account's wording
/// names the relink, since that is the half a running session feels.
///
/// Both halves split again on what the clear falls back TO. The gate passes on
/// EITHER credential, so an api-key account with a sidecar clears fine onto an
/// absent install source and is signed out rather than relinked; the hint
/// promised a relink in both states until 2026-08-12. The two api-key legs are
/// what make `clear_falls_back_to_oauth` load-bearing rather than a restatement
/// of `has_other_login`.
#[test]
fn clear_session_token_hint_names_the_gate_then_what_the_clear_falls_back_to() {
    let mut snap = Snap::blank("a");

    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some("cannot clear with no other login stored"),
    );

    // An api key alone opens the gate, and there is no login behind it.
    snap.has_other_login = true;
    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some("the next switch runs this account on its API key"),
    );

    snap.is_active = true;
    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some("signs Claude Code out now · this account runs on its API key"),
    );

    // A stored OAuth pair is what the clear actually falls back to.
    snap.clear_falls_back_to_oauth = true;
    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some("relinks this account's own login now · running sessions follow"),
    );

    snap.is_active = false;
    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some("the next switch installs this account's own login again"),
    );

    snap.is_active = true;
    // The full-scope disclosure: this hint is the TUI's ONLY statement that a
    // clear on a rolling profile stops the re-stamping and destroys the
    // preserved mint — the CLI prints two explicit lines for the same act,
    // and a two-press arm is not a disclosure. (The gate-vs-flag-only split
    // has its own test below.)
    snap.rolling_armed = true;
    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some("relinks this account's own login now · running sessions follow · re-stamping stops"),
    );
    snap.has_static_backup = true;
    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some(
            "relinks this account's own login now · running sessions follow · re-stamping \
             stops · the preserved mint goes too"
        ),
    );
    // The disarm half also fires off the sidecar CONTENT alone (a rolling
    // bearer with the flag raced off), so neither signal can silently stop
    // carrying the disclosure.
    snap.rolling_armed = false;
    snap.rolling_token = true;
    snap.has_static_backup = false;
    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some("relinks this account's own login now · running sessions follow · re-stamping stops"),
    );
}

/// The gate arm reads the SAME condition as `run_config_row`'s refusal
/// (`Snap::clear_gated`): a flag-only account (armed, nothing stamped, no
/// preserved mint) disarms without stripping a credential, so its hint must
/// describe the act — with its OWN copy, since the 4-way base's api-key arms
/// would promise a credential this account does not hold. The moment a stored
/// piece exists, the gate line is back — clearing THAT would strip the last
/// credential.
#[test]
fn clear_session_token_hint_lets_a_flag_only_account_past_the_gate() {
    let mut snap = Snap::blank("a");
    snap.rolling_armed = true;

    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some("stops the daemon re-stamping this account · nothing else is stored"),
    );

    // Active, the relink onto an absent install source signs Claude Code out,
    // and the hint says so instead of hiding it behind the disarm.
    snap.is_active = true;
    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some(
            "stops the daemon re-stamping this account · signs Claude Code out · nothing is \
             stored behind it"
        ),
    );
    snap.is_active = false;

    snap.session_token = Some(crate::claude::SessionTokenStatus::LongLived(None));
    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some("cannot clear with no other login stored"),
    );

    snap.session_token = None;
    snap.has_static_backup = true;
    assert_eq!(
        row_hint(ConfigRow::ClearSessionToken, &snap).as_deref(),
        Some("cannot clear with no other login stored"),
    );
}

/// `build_snap` derives the token row from what the sidecar HOLDS, never from
/// the config flag. The two part ways exactly when honesty matters: a dead
/// chain degrades the sidecar onto its static mint while the flag stays on,
/// and a flag-driven row would promise a re-stamp in ~8760h for a mint nobody
/// is going to re-stamp — the same comfortable-looking lie the honest
/// countdown exists to prevent, from the other direction.
#[test]
fn snap_rolling_token_is_the_sidecar_content_not_the_config_flag() {
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken};
    let _home = crate::testutil::HomeSandbox::new();
    let name = "cfg-snap-roll";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let app_with_flag = |rolling_token: bool| {
        let mut p = crate::profile::Profile::new(name.to_string(), None, None);
        p.rolling_token = rolling_token;
        App::new(AppConfig {
            state: AppState {
                profiles: vec![p.name.clone()],
                ..AppState::default()
            },
            profiles: vec![p],
        })
    };
    let sidecar = |scopes: Vec<&str>, plan: Option<&str>| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat01-snap-fixture".to_string(),
            refresh_token: None,
            expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
            scopes: Some(scopes.into_iter().map(String::from).collect()),
            subscription_type: plan.map(String::from),
            ..crate::profile::OAuthToken::default_extra()
        }),
    };

    // Flag ON, sidecar degraded onto the mint: the row must say mint.
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&sidecar(
            vec!["user:inference", "user:sessions:claude_code"],
            None,
        ))
        .expect("ser"),
    )
    .expect("write mint");
    assert!(
        !build_snap(&app_with_flag(true), true).rolling_token,
        "a degraded profile must render the mint it is actually on"
    );

    // Flag OFF, sidecar holding a rolling bearer (`clauth static-token` flips
    // the flag before the restore lands): the row must say rolling.
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&sidecar(
            vec!["user:inference", "user:profile"],
            Some("max"),
        ))
        .expect("ser"),
    )
    .expect("write rolling");
    assert!(
        build_snap(&app_with_flag(false), true).rolling_token,
        "what sessions actually hold outranks the flag in both directions"
    );
}

/// The ROLLING half of the token row, previously untested end to end: the
/// countdown counts to the RE-STAMP (expiry minus `ROLLING_RESTAMP_HORIZON_MS`),
/// never to the expiry — the leg renews 2h ahead, so an expiry-based label
/// read 2h high everywhere except zero — and the stalled state names the
/// re-arm command with the live profile name, house `·` form.
#[test]
fn rolling_token_row_counts_to_the_restamp_and_escalates() {
    use crate::claude::SessionTokenStatus as S;
    let now = crate::usage::now_ms() as i64;
    let hour = 3_600_000i64;
    let w = 60usize;
    let text = |ls: &[Line<'static>]| -> String {
        ls.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect()
    };

    // 5h of bearer life = 3h until the 2h-ahead re-stamp fires.
    let fresh = session_token_lines(&S::LongLived(Some(now + 5 * hour)), true, "acct", now, w);
    assert_eq!(fresh.len(), 1);
    assert!(
        text(&fresh).contains("rolling · re-stamps in ~3h"),
        "{}",
        text(&fresh)
    );

    // Inside the horizon: the re-stamp is due NOW, and saying "~0h" would
    // read as a countdown that stopped.
    let due = session_token_lines(&S::LongLived(Some(now + hour)), true, "acct", now, w);
    assert!(
        text(&due).contains("rolling · re-stamp due"),
        "{}",
        text(&due)
    );

    // 2.5h of life = 30 minutes until the re-stamp: STRICTLY inside the
    // sub-hour band, not on its zero boundary. The boundary cases alone let
    // the band predicate collapse to `== 0` unnoticed — measured — and this
    // fixture is what a collapsed predicate renders as a stopped-looking
    // "~0h" countdown.
    let inside = session_token_lines(
        &S::LongLived(Some(now + 2 * hour + hour / 2)),
        true,
        "acct",
        now,
        w,
    );
    assert!(
        text(&inside).contains("rolling · re-stamp due"),
        "{}",
        text(&inside)
    );

    // Expired: the stalled DANGER state, with the fix line naming the live
    // profile and the re-arm verb.
    let stalled = session_token_lines(&S::LongLived(Some(now - hour)), true, "acct", now, w);
    // Whitespace-normalized: the tooltip wraps, padding line breaks with
    // spaces mid-sentence.
    let stalled_t = text(&stalled)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(stalled_t.contains("rolling token stalled"), "{stalled_t}");
    assert!(
        stalled_t.contains("clauth rolling-token acct"),
        "the fix line interpolates the profile, never a <p> placeholder: {stalled_t}"
    );

    // No recorded expiry: named as rolling, not as the mint.
    let unstamped = session_token_lines(&S::LongLived(None), true, "acct", now, w);
    assert!(
        text(&unstamped).contains("rolling · no recorded expiry"),
        "{}",
        text(&unstamped)
    );
}

/// The stalled-rolling fix line interpolates `snap.title`, never `snap.name`:
/// `build_snap(app, draft.is_none())` blanks `name` whenever a draft is open,
/// so a name-fed line renders `clauth rolling-token  re-arms` — a fix command
/// with a hole where the profile belongs — exactly while the operator is
/// editing the profile it names. Driven through `draw_settings_rows` with the
/// draft-open Snap shape (`title` carries the profile, `name` blank), which is
/// the shape the direct `session_token_lines` tests above never exercise.
#[test]
fn stalled_rolling_fix_line_uses_the_title_that_survives_a_draft() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::profile::{AppConfig, AppState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut snap = Snap::blank("acct");
    snap.rolling_token = true;
    snap.session_token = Some(crate::claude::SessionTokenStatus::LongLived(Some(
        crate::usage::now_ms() as i64 - 3_600_000,
    )));
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    term.draw(|f| draw_settings_rows(f, f.area(), &app, &[], 0, &snap, false))
        .unwrap();
    let joined = crate::testutil::buffer_rows(term.backend().buffer())
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("clauth rolling-token acct re-arms"),
        "the fix line reads the title, which a draft never blanks: {joined}"
    );
}

/// One login row, one pair of labels: `+ login` until a credential exists,
/// `re-login` after, whichever of its three flows the account routes to.
#[test]
fn login_labels_read_the_same_for_every_flow() {
    let input = InputState::new("");
    let text = |snap: &Snap, row: ConfigRow| {
        line_text(&one_row(row, false, false, None, snap, &input))
            .trim()
            .to_string()
    };
    let mut snap = Snap::blank("a");
    assert_eq!(text(&snap, ConfigRow::Login), "+ login");
    snap.logged_in = true;
    assert_eq!(text(&snap, ConfigRow::Login), "re-login");

    // An api-key account: the row re-enters the key.
    snap.login_is_oauth = false;
    assert_eq!(text(&snap, ConfigRow::Login), "re-login");
    snap.logged_in = false;
    assert_eq!(text(&snap, ConfigRow::Login), "+ login");

    // A Model Studio account is api-typed too, and its row opens the console.
    snap.login_is_oauth = true;
    snap.console_login = true;
    assert_eq!(text(&snap, ConfigRow::Login), "+ login");
    snap.logged_in = true;
    assert_eq!(text(&snap, ConfigRow::Login), "re-login");
}

/// cloudy-tui "Keys and commands inside prose" on the token row: both fix
/// lines' commands render ACCENT, never bold.
#[test]
fn session_token_fix_lines_style_their_commands() {
    use crate::claude::SessionTokenStatus as S;
    use crate::testutil::{assert_prose_part, lines_buffer};
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let now = 1_700_000_000_000_i64;
    let hour = 3_600_000_i64;
    let w = 100;

    let dead = session_token_lines(&S::LongLived(Some(now - hour)), false, "acct", now, w);
    assert_prose_part(
        &lines_buffer(dead, 100),
        "re-mint with claude setup-token",
        "claude setup-token",
        false,
    );
    let stalled = session_token_lines(&S::LongLived(Some(now - hour)), true, "acct", now, w);
    assert_prose_part(
        &lines_buffer(stalled, 100),
        "· clauth rolling-token acct re-arms",
        "clauth rolling-token acct",
        false,
    );
}

/// The `alert at` row at rest shows the SAVED value (the snapshot), never a
/// stale draft buffer: the percent in the accent while set, a faint `off`
/// while not; the editor shows the raw buffer; the hint names each state.
#[test]
fn the_alert_row_shows_its_percent_or_off_and_hints_each_state() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut snap = Snap::blank("a");
    let stale_draft = InputState::new("40");

    snap.bell_threshold = "90".into();
    let set = one_row(
        ConfigRow::BellThreshold,
        true,
        false,
        None,
        &snap,
        &stale_draft,
    );
    assert_eq!(set.spans[1].content.trim_end(), "alert at");
    assert_eq!(set.spans[2].content, "90%");
    assert_eq!(set.spans[2].style.fg, theme::accent().fg);
    assert_eq!(
        row_hint(ConfigRow::BellThreshold, &snap).unwrap(),
        "alerts once when 5h usage reaches this, again after it drops below"
    );

    snap.bell_threshold = "97.5".into();
    let fraction = one_row(
        ConfigRow::BellThreshold,
        false,
        false,
        None,
        &snap,
        &stale_draft,
    );
    assert_eq!(fraction.spans[2].content, "97.5%");

    snap.bell_threshold = String::new();
    let off = one_row(
        ConfigRow::BellThreshold,
        false,
        false,
        None,
        &snap,
        &stale_draft,
    );
    assert_eq!(off.spans[2].content, "off");
    assert_eq!(off.spans[2].style.fg, theme::faint().fg);
    assert_eq!(
        row_hint(ConfigRow::BellThreshold, &snap).unwrap(),
        concat!(
            "no alert when 5h usage climbs; ",
            key_lit!("↵"),
            " or ",
            key_lit!("+"),
            " to set one"
        )
    );

    let editing = one_row(
        ConfigRow::BellThreshold,
        true,
        true,
        None,
        &snap,
        &InputState::new("8"),
    );
    assert_eq!(editing.spans[1].content.trim_end(), "alert at");
    assert_eq!(
        editing.spans[2].content, "8",
        "the editor shows the raw buffer"
    );
    assert_eq!(editing.spans[2].style.fg, theme::body().fg);
    assert_eq!(editing.spans[3].content, " %");
    assert_eq!(editing.spans[3].style.fg, theme::faint().fg);

    let refused = one_row(
        ConfigRow::BellThreshold,
        true,
        true,
        None,
        &snap,
        &InputState::new("101"),
    );
    assert_eq!(refused.spans[2].content, "101");
    assert_eq!(refused.spans[2].style.fg, theme::danger().fg);
    assert_eq!(refused.spans[3].style.fg, theme::danger().fg);
}

/// While typing, the `alert at` field carries its accepted range under it,
/// faint while the buffer would save and DANGER while it would be refused.
#[test]
fn the_alert_editor_names_its_range_and_turns_danger_on_a_refused_buffer() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    for (raw, refused) in [
        ("90", false),
        ("", false),
        ("0", false),
        ("100", false),
        ("101", true),
        ("abc", true),
        ("-1", true),
    ] {
        let lines = alert_range_tooltip(&InputState::new(raw), WIDE);
        assert_eq!(lines.len(), 1, "{raw:?}");
        assert_eq!(
            line_text(&lines[0]),
            " └ 0-100 % · empty turns it off",
            "{raw:?}"
        );
        let want = if refused {
            theme::danger().fg
        } else {
            theme::faint().fg
        };
        assert_eq!(lines[0].spans[1].style.fg, want, "{raw:?}");
    }
}

/// A control character in an edited value renders escaped in the Setup editor
/// body, and the shared caret-column count matches that escaped body, so the
/// native cursor never drifts right of the caret.
#[test]
fn a_control_character_value_renders_escaped_in_the_setup_editor() {
    let input = InputState::new("x\ty");
    let spans = value_spans(&input, true, false);
    let body: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(
        body, "x\\ty",
        "the editor body escapes the control character"
    );
    assert_eq!(
        head_cols(&input),
        body.chars().count(),
        "the caret column counts the escaped body"
    );
}
