//! Bottom strip: key hints, or a footer alert in place when one is active.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, CodexDetailRow, CodexLoginSession, ConfigEntryRef, ConfigFocus, ConfigRow, FallbackHint,
    FallbackRow, FooterAlert, GLOBAL_CONFIG_ROWS, GlobalConfigRow, HERDR_OPTIONS, HerdrOption,
    KeyOwner, LoginSession, LossWarningTarget, MainItemKind, Modal, PaneState, PoolDetailRow,
    PoolMutationKind, ScrollStamp, ServiceFix, ServicesFocus, SetupSelection, ShuntEntryKey,
    ShuntFieldEdit, ShuntFieldKey, ShuntFocus, ShuntSaveKey, StatusFocus, Tab, TokenView,
    action_menu_opens, chain_candidates, chain_enter_acts, chain_items, chain_reorder_moves,
    config_rows, config_step_lands, config_view, detail_scrolls, disable_row_gated, entry_for_key,
    escape_control, fallback_hint, fallback_row_acts, fallback_step_lands, field_locked, fix_verb,
    global_config_row_live, has_sub_focus, herdr_config_writable, herdr_tag_step, is_cycle_field,
    keyboard_owner, login_row_refused, main_reorder_moves, minted_key_row, overview_edit_acts,
    page_keys_act, pane_state, rolling_switch_row_gated, services_fix_keys_act, token_model_count,
    walk_moves,
};
use super::super::theme;
use super::format::{
    grapheme_cells, prefix_in_cells, spinner_frame, trim_trailing_ellipsis_graphemes, truncate,
};
use super::prose;
use super::services::{field_label, route_label};

const TAB_NAV: (&str, &str) = ("←→", "tabs");

/// A typed field's whole grammar: `q` is data there, so it gets no hint. Esc
/// puts the field back to its saved value, and says so in the field's own
/// terms, since beside a login in flight a bare `cancel` reads as the login's.
const TYPED_FIELD: &[(&str, &str)] = &[("↵", "save"), ("←→", "caret"), ("esc", "revert")];

/// A shunt cycle row's pending pick: no caret to walk, so it sheds `←→ caret`.
const SHUNT_PICK: &[(&str, &str)] = &[("↵", "save"), ("esc", "revert")];

/// A draft field's typed editor: nothing saves until the form's own
/// `save <kind>` row, so `↵` reads `done` (the Setup `+ new` form's word); its
/// `esc` restores the draft value (unlike that form's `esc done`), so it keeps
/// `esc revert`.
const DRAFT_FIELD: &[(&str, &str)] = &[("↵", "done"), ("←→", "caret"), ("esc", "revert")];

/// A draft cycle row's pending pick: the `done` word, no caret to walk.
const DRAFT_PICK: &[(&str, &str)] = &[("↵", "done"), ("esc", "revert")];

/// A field editor whose row is saving is locked: only the editing keys (`esc`,
/// typing, `↵`, `space`) are inert, so the footer names the non-edit keys that
/// still act — `↑↓` moves off the row and `q` backs out (closing the editor) —
/// and drops every edit hint. `?` and `x` fall through the owner, so `? help`
/// is derived from what the owner claims and `x` (dismiss a toast) keeps its
/// app-wide no-bar-hint convention.
const LOCKED: &[(&str, &str)] = &[("↑↓", "row"), ("q", "back")];

/// The `+ new` form's typed field: ⏎ and esc both end the edit and keep the
/// typed value; nothing saves until the form's own `create account` row.
const NEW_ACCOUNT_FIELD: &[(&str, &str)] = &[("↵", "done"), ("←→", "caret"), ("esc", "done")];

/// The note editor's grammar: ⏎ saves the draft, ⌃j inserts a newline, esc
/// cancels. ←→ move the caret inside the draft (never switch tabs).
const NOTE_EDITOR_FIELD: &[(&str, &str)] = &[
    ("↵", "save"),
    ("⌃j", "newline"),
    ("←→", "caret"),
    ("esc", "cancel"),
];

/// The help modal's own keys while its keymap overflows: ↑↓ scrolls, `q` (or
/// `esc` / `?`) closes. The modal's own sections already document both, so the
/// footer keeps the two discoverable keys alone; while the body fits, `q back`
/// alone acts — a key that scrolls nothing advertises nothing.
const HELP_KEYS: &[(&str, &str)] = &[("↑↓", "scroll"), ("q", "back")];

/// The read-only `check output` modal scrolls and closes like the help modal.
const CHECK_OUTPUT_KEYS: &[(&str, &str)] = &[("↑↓", "scroll"), ("q", "back")];

/// The lone `q back` of a scroll modal whose body fits.
const Q_BACK: &[(&str, &str)] = &[("q", "back")];

/// A two-button confirm: ←→ (or tab) moves the highlight between the buttons,
/// ↵ commits the highlighted one, `q` backs out.
const BUTTON_KEYS: &[(&str, &str)] = &[("←→", "pick"), ("↵", "choose"), ("q", "back")];

/// The loss warning's three buttons: ←→ steps and tab wraps between them, ↵
/// chooses the highlighted one, `esc` and `q` both `go back`.
const LOSS_WARNING_KEYS: &[(&str, &str)] =
    &[("←→", "pick"), ("↵", "choose"), ("esc · q", "go back")];

/// The same confirm with `↑↓ scroll` after the choose keys when its body
/// overflows, so a narrow terminal sheds the scroll hint before the choice
/// keys.
const BUTTON_SCROLL_KEYS: &[(&str, &str)] = &[
    ("←→", "pick"),
    ("↵", "choose"),
    ("↑↓", "scroll"),
    ("q", "back"),
];

/// A vertical option list: ↑↓ moves the cursor, ↵ picks the row under it,
/// `q` backs out.
const LIST_KEYS: &[(&str, &str)] = &[("↑↓", "pick"), ("↵", "choose"), ("q", "back")];

/// The preset picker adds `d` (delete a saved preset; the body also teaches
/// it) and ↵ applies the picked preset. A built-in row has no file to drop, so
/// its footer drops `d delete` — the same predicate the `d` handler reads.
const PRESET_PICKER_KEYS: &[(&str, &str)] = &[
    ("↑↓", "pick"),
    ("↵", "apply"),
    ("d", "delete"),
    ("q", "back"),
];

/// The preset picker over a built-in row: no `d delete`.
const PRESET_PICKER_BUILTIN_KEYS: &[(&str, &str)] =
    &[("↑↓", "pick"), ("↵", "apply"), ("q", "back")];

/// A name-entry modal that closes on `q`: ↵ saves the name, ←→ moves the
/// caret, `q` backs out.
const CAPTURE_NAME_KEYS: &[(&str, &str)] = &[("↵", "save"), ("←→", "caret"), ("q", "back")];

/// A name-entry modal where `q` is data: ↵ saves the name, ←→ moves the
/// caret, `esc` goes back (closes it) — `back`, never `cancel`, so beside a
/// login in flight the word can't read as cancelling the login.
const NAME_PROMPT_KEYS: &[(&str, &str)] = &[("↵", "save"), ("←→", "caret"), ("esc", "back")];

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // The footer row is the last full-width row of the frame it belongs to;
    // the scroll-bound stamps record that frame's area, so the footer compares
    // against it and drops a resize's stale bound.
    let frame_area = Rect::new(0, 0, area.width, area.y + area.height);

    // 1-col breathing room on each side; the alert row (which replaces this in
    // place) shares the same inset so the left margin never jumps.
    let area = inset_x(area, 1);

    let owner = keyboard_owner(app);

    // The save cue is a state mirror, computed each frame (not a stored alert):
    // it shows while the quit waits for a running write — the exit wait, or a
    // quit held on a checking field/`+ add` save — on every tab, and `x` cannot
    // dismiss it. It outranks any stored alert.
    if app.exit_wait || app.loss_pending == Some(LossWarningTarget::Quit) {
        let (verb, row) = save_cue_row(app);
        let msg = save_cue(verb, &row, area.width);
        draw_alert(frame, area, SAVE_CUE_LEADER, theme::info(), &msg);
        return;
    }

    // A live alert replaces the footer in place — one footer row, never
    // stacked — and outranks the login line: a ctrl+c arm can happen while a
    // login runs, and the quit prompt must be visible then.
    if let Some(alert) = &app.footer_alert {
        let FooterAlert::Warn(msg) = alert;
        draw_alert(frame, area, "! ", theme::warning(), msg);
        return;
    }

    // A login in flight owns the footer while no alert is live.
    if let Some(session) = &app.login {
        // An open editor takes esc/q before the login's cancel does, so its own
        // grammar is the hint. Any open modal owns esc/q too (the login modal
        // collapses, others handle their own keys), so the hint is that modal's
        // own row behind the login lead; the login modal's open code field is
        // the exception, where `q` is data and ↵ submits.
        let keys = login_keys(owner, app, frame_area, session.paste_field.is_some());
        draw_login(frame, area, session, keys, owner, app);
        return;
    }

    // A codex browser re-login in flight owns the footer the same way: the
    // modal is the live view, this row is the collapsed one carrying the name.
    // As on the claude login line, any open non-login modal owns esc/q first,
    // so its own row rides behind the codex lead; collapsed, esc cancels.
    if let Some(session) = &app.codex_login {
        let keys = login_keys(owner, app, frame_area, false);
        draw_codex_login(frame, area, session, keys, owner, app);
        return;
    }

    // An editor owns the keys it claims, `←→` included, so its own grammar is
    // the whole row: `q` only where the editor itself binds it, `? help` only
    // where it lets `?` through. Every owner takes the arrows, so `←→ tabs`
    // rides only while nothing owns the keyboard.
    let mut hints: Vec<(&str, &str)> =
        match owner.zip(owner.and_then(|o| owner_hints(o, app, frame_area))) {
            Some((owner, own)) => {
                let mut hints = own.to_vec();
                insert_help(&mut hints, Some(owner), app);
                hints
            }
            None => owner
                .is_none()
                .then_some(if pane_state(app) == PaneState::Rest {
                    TAB_NAV
                } else {
                    ("←", "back")
                })
                .into_iter()
                .chain(tab_hints(app))
                .collect(),
        };

    // `a` opens nothing where the menu is empty: a tab with no action of its
    // own while a daemon start or stop is in flight and no shunt verb
    // applies. Reading the real menu
    // keeps the hint honest per row instead of leaving each arm's literal to
    // drift.
    if !action_menu_opens(app) {
        hints.retain(|(key, _)| *key != "a");
    }

    // `e` opens the cursor's Setup editor and nothing without a row under the
    // cursor that resolves to an account; `⇧↑↓` swaps the cursor's account
    // with a neighbour. Each reads the predicate its handler acts on.
    if app.tab == Tab::Overview && owner.is_none() && !overview_edit_acts(app) {
        hints.retain(|(key, _)| *key != "e");
    }
    if app.tab == Tab::Overview && owner.is_none() && !main_reorder_moves(app) {
        hints.retain(|(key, _)| *key != "⇧↑↓");
    }

    shed_to_width(&mut hints, area.width as usize);

    let mut spans: Vec<Span<'_>> = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", theme::faint()));
        }
        spans.push(Span::styled(*key, theme::accent().bold()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, theme::dim()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}

/// A published scroll bound is this frame's only when its stamp still matches
/// the frame's area and the modal-open generation: a resize changes the area,
/// and an open or close changes the generation, so a bound left over from
/// either errs the footer to the bound's no-scroll row.
fn bound_is_current(stamp: ScrollStamp, area: Rect, app: &App) -> bool {
    stamp.width == area.width
        && stamp.height == area.height
        && stamp.generation == app.modal_generation.get()
}

/// The `? help` an owner's row derives: [`owner_hints`] rows omit it, and this
/// inserts it ahead of the owner's `q` group wherever `?` falls through to the
/// global help arm — an owner that lets `?` past (the day picker, the re-bind
/// confirm, a locked shunt editor), or no owner at all (the login lines'
/// collapsed row; the plain footer's no-owner row carries the pair in
/// [`tab_hints`] instead). The login lines retire the pair where their shed
/// row cannot hold it: [`login_row_hints`].
fn insert_help(hints: &mut Vec<(&'static str, &'static str)>, owner: Option<KeyOwner>, app: &App) {
    if !owner.is_some_and(|owner| owner.claims(app, KeyCode::Char('?'))) {
        let at = hints
            .iter()
            .position(|(key, _)| *key == "q")
            .unwrap_or(hints.len());
        hints.insert(at, ("?", "help"));
    }
}

/// An owner's own grammar, the whole hint row while it holds the keyboard
/// bar the `? help` that [`draw`] derives from what the owner claims. For the
/// modal owner the row is read off the top modal's real handler, one arm per
/// modal, so a new modal that skips its arm fails to compile. `None` only for
/// the two login modals, whose login-in-flight footers [`draw_login`] and
/// `draw_codex_login` own.
fn owner_hints(
    owner: KeyOwner,
    app: &App,
    area: Rect,
) -> Option<&'static [(&'static str, &'static str)]> {
    match owner {
        KeyOwner::Modal => match app.modals.last() {
            // `↑↓ scroll` rides only while the body actually overflows — the
            // same published bound the scroll handler clamps against — and the
            // bound is this frame's, so a key that scrolls nothing, or a bound
            // a resize or reopen left stale, advertises nothing.
            Some(Modal::Help)
                if app.help_max_scroll.get() > 0
                    && bound_is_current(app.help_max_scroll_stamp.get(), area, app) =>
            {
                Some(HELP_KEYS)
            }
            Some(Modal::Help) => Some(Q_BACK),
            Some(Modal::CheckOutput(_))
                if app.check_output_max_scroll.get() > 0
                    && bound_is_current(app.check_output_max_scroll_stamp.get(), area, app) =>
            {
                Some(CHECK_OUTPUT_KEYS)
            }
            Some(Modal::CheckOutput(_)) => Some(Q_BACK),
            Some(Modal::Confirm(state))
                if state.scroll.max_scroll.get() > 0
                    && bound_is_current(state.scroll.stamp.get(), area, app) =>
            {
                Some(BUTTON_SCROLL_KEYS)
            }
            Some(Modal::Confirm(_)) => Some(BUTTON_KEYS),
            // The loss warning's three buttons never scroll: it stays the plain
            // button row, with `esc` named beside `q` as `go back`.
            Some(Modal::LossWarning(_)) => Some(LOSS_WARNING_KEYS),
            Some(Modal::PresetApply(form))
                if form.scroll.max_scroll.get() > 0
                    && bound_is_current(form.scroll.stamp.get(), area, app) =>
            {
                Some(BUTTON_SCROLL_KEYS)
            }
            Some(Modal::PresetApply(_)) => Some(BUTTON_KEYS),
            Some(Modal::EnvCollision(state)) => {
                if state.reviewing_overwrite {
                    if state.max_scroll.get() > 0
                        && bound_is_current(state.max_scroll_stamp.get(), area, app)
                    {
                        Some(BUTTON_SCROLL_KEYS)
                    } else {
                        Some(BUTTON_KEYS)
                    }
                } else {
                    Some(LIST_KEYS)
                }
            }
            Some(Modal::Divergence(_) | Modal::DivergenceTarget(_) | Modal::TokenPicker(_)) => {
                Some(LIST_KEYS)
            }
            // Its `s`/`p`/`c` keys sit in its own rows, the action menu's
            // hotkey shape.
            Some(
                Modal::ActionMenu(_)
                | Modal::BareItem(_)
                | Modal::BareItemOwner(_)
                | Modal::CaptureOwner(_),
            ) => Some(LIST_KEYS),
            // `d delete` drops on a built-in row, whose `d` only toasts a
            // refusal — the same predicate the `d` handler reads.
            Some(Modal::PresetPicker(state))
                if state
                    .presets
                    .get(state.cursor.min(state.presets.len().saturating_sub(1)))
                    .is_some_and(|p| !p.builtin) =>
            {
                Some(PRESET_PICKER_KEYS)
            }
            Some(Modal::PresetPicker(_)) => Some(PRESET_PICKER_BUILTIN_KEYS),
            Some(Modal::CaptureName(_)) => Some(CAPTURE_NAME_KEYS),
            Some(Modal::NamePrompt(_)) => Some(NAME_PROMPT_KEYS),
            // The re-bind confirm walks an account list the cursor steps over,
            // so its ruled copy leads `↑↓ row`; `?` falls through it so
            // [`draw`] derives `? help` beside the row.
            Some(Modal::Rebind(_)) => Some(&[
                ("↑↓", "row"),
                ("space", "toggle"),
                ("↵", "choose"),
                ("q", "not now"),
            ]),
            Some(Modal::Login | Modal::CodexLogin) | None => None,
        },
        KeyOwner::SetupField
        | KeyOwner::MemberField
        | KeyOwner::RefreshInterval
        | KeyOwner::ContextNudge
        | KeyOwner::WeeklyThreshold
        | KeyOwner::HerdrTag => Some(TYPED_FIELD),
        KeyOwner::NewAccountField => Some(NEW_ACCOUNT_FIELD),
        KeyOwner::NoteEditor => Some(NOTE_EDITOR_FIELD),
        // The shunt field editor reuses the typed field's grammar; a pending
        // cycle pick has no caret to walk, so it sheds `←→ caret`. While the
        // row's save checks, its editor is locked: the edit keys are inert
        // (no `esc revert` / `↵ save`), `↑↓` still moves off the row and `q`
        // ascends, and the outcome surfaces as a toast or the `└` line.
        KeyOwner::ShuntField => match app.services.shunt_field_edit.as_ref() {
            Some(state) if field_locked(app, &state.key) => Some(LOCKED),
            Some(state) if matches!(state.edit, ShuntFieldEdit::Pick { .. }) => {
                if matches!(state.key, ShuntFieldKey::Draft { .. }) {
                    Some(DRAFT_PICK)
                } else {
                    Some(SHUNT_PICK)
                }
            }
            Some(state) if matches!(state.key, ShuntFieldKey::Draft { .. }) => Some(DRAFT_FIELD),
            _ => Some(TYPED_FIELD),
        },
        // `←→` walk the chips and each space saves, so nothing reads as a
        // commit; `q` leaves the picker. ⏎ leaves it too, so it takes no group
        // of its own beside `q back`: three screen-specific groups at most.
        KeyOwner::DayPicker => Some(&[
            ("←→", "day"),
            ("space", "toggle"),
            ("↑↓", "row"),
            ("q", "back"),
        ]),
    }
}

/// The screen's own keys, then `q`: the row under `←→ tabs` while nothing owns
/// the keyboard. A modal's row comes from [`owner_hints`] instead, so this
/// never renders under one.
fn tab_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    // `q` label: "back" in a sub-focus, "quit" at top level.
    // (While armed the alert row shows instead, so this label stays "quit".)
    let q_label: &str = if has_sub_focus(app) { "back" } else { "quit" };

    // The Services hints carry a per-fix verb (a computed label), so they build
    // a Vec instead of one of the static `&[...]` arms below.
    if app.tab == Tab::Services {
        let mut hints = services_hints(app);
        hints.push(("q", q_label));
        return hints;
    }

    // The Usage hints derive per row (a codex row has no `n`/`r`; a pool row's
    // `r` re-reads the listing), so they build a Vec like Services.
    if app.tab == Tab::Usage {
        let mut hints = usage_hints(app);
        hints.push(("q", q_label));
        return hints;
    }

    // The Status list hints derive from the incident count like Services: the
    // key handler gates `↑↓` and `↵` on the same count this reads, and a lone
    // incident's walk wraps onto itself, so neither shows where it acts on
    // nothing.
    if app.tab == Tab::Status && app.status.focus == StatusFocus::List {
        let mut hints = Vec::new();
        if walk_moves(app.status.incidents.len()) {
            hints.push(("↑↓", "incident"));
        }
        if !app.status.incidents.is_empty() {
            hints.push(("↵", "open"));
        }
        hints.extend([("r", "refresh"), ("a", "actions"), ("?", "help")]);
        hints.push(("q", q_label));
        return hints;
    }

    let mut hints: Vec<(&'static str, &'static str)> = match app.tab {
        Tab::Overview => vec![
            ("⇧↑↓", "reorder"),
            ("e", "edit"),
            ("a", "actions"),
            ("c", "harness"),
            ("?", "help"),
        ],
        Tab::Usage => Vec::new(), // handled above: its hints derive per row
        Tab::Tokens => token_hints(app),
        Tab::Setup => match app.config_focus {
            ConfigFocus::Profiles => {
                // `+ new` is a row of the walk, so an empty roster's lone row
                // has nowhere to go.
                let mut hints = Vec::new();
                if walk_moves(app.setup_items().len() + 1) {
                    hints.push(("↑↓", "account"));
                }
                hints.extend([
                    ("↵", "configure"),
                    ("n", "new"),
                    ("a", "actions"),
                    ("?", "help"),
                ]);
                hints
            }
            ConfigFocus::Actions => setup_card_hints(app),
        },
        Tab::Config => config_hints(app),
        Tab::Status => match app.status.focus {
            StatusFocus::List => Vec::new(), // handled above: its hints derive from the incident count
            StatusFocus::Detail => {
                let mut hints = Vec::new();
                if detail_scrolls(app) {
                    hints.push(("↑↓", "scroll"));
                }
                hints.extend([("a", "actions"), ("?", "help")]);
                hints
            }
        },
        Tab::Services => Vec::new(), // handled above: its hints carry a computed verb
        Tab::Fallback => fallback_hints(app),
    };

    // An armed 2-step's way out is its `esc cancel`: `q` ascends out of the card
    // the same way there, so `q back` would be a second name for it. The codex
    // and pool details arm the same way, so they skip `q` too.
    let setup_armed = app.tab == Tab::Setup
        && app.config_focus == ConfigFocus::Actions
        && (app.codex_detail_armed
            || app.pool_detail_armed
            || setup_delete_armed(app)
            || setup_token_row_armed(app, ConfigRow::ClearSessionToken)
            || setup_token_row_armed(app, ConfigRow::RollingTokenSwitch));
    if !(setup_armed
        || (app.tab == Tab::Fallback && fallback_hint(app) == FallbackHint::DetailRemoveArmed))
    {
        hints.push(("q", q_label));
    }
    hints
}

/// The Tokens tab's own hints: `↑↓` scrolls a model detail only while it has
/// somewhere to scroll and walks the model list only past one model, the
/// bound and the count the key handler steps over.
fn token_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    const LENSES: [(&str, &str); 4] = [
        ("c", "count cache"),
        ("t", "period"),
        ("a", "actions"),
        ("?", "help"),
    ];
    let mut hints = Vec::new();
    match app.token_view {
        TokenView::Dashboard if app.tokens_failed => {
            return vec![
                ("↵", "cause"),
                ("r", "reload"),
                ("a", "actions"),
                ("?", "help"),
            ];
        }
        TokenView::Dashboard => {
            if token_model_count(app) > 0 {
                hints.push(("↵", "models"));
            }
            hints.push(("r", "reload"));
        }
        TokenView::Models if app.model_detail_focus == StatusFocus::Detail => {
            if detail_scrolls(app) {
                hints.push(("↑↓", "scroll"));
            }
        }
        TokenView::Models => {
            let len = token_model_count(app);
            if walk_moves(len) {
                hints.push(("↑↓", "model"));
            }
            if len > 0 {
                hints.push(("↵", "detail"));
            }
        }
    }
    hints.extend(LENSES);
    hints
}

/// The Setup card's armed `delete account`: its second `↵` deletes.
fn setup_delete_armed(app: &App) -> bool {
    config_rows(app).get(app.config_action_cursor) == Some(&ConfigRow::Delete)
        && app
            .config_draft
            .as_ref()
            .is_some_and(|d| d.armed_action == Some(ConfigRow::Delete))
}

/// What `↵` does on the Setup card's `disable` row: `enable` on a disabled
/// account, `disable` (armed first) otherwise, `None` while the row is inert
/// — the account is active or holds a live session.
pub(super) fn setup_disable_verb(app: &App) -> Option<&'static str> {
    let name = crate::profile::ProfileName::from(app.config_draft.as_ref()?.editing_name.clone()?);
    if disable_row_gated(app, &name) {
        return None;
    }
    let disabled = app
        .config()
        .find(&name)
        .is_some_and(crate::profile::Profile::is_disabled);
    Some(if disabled { "enable" } else { "disable" })
}

/// Whether the Setup card's `clear long-lived token` / `switch to a rolling
/// token` row under the cursor is armed (its second `↵` acts). The arm
/// disarms when focus moves, so the cursor check matches the held arm.
fn setup_token_row_armed(app: &App, row: ConfigRow) -> bool {
    config_rows(app).get(app.config_action_cursor) == Some(&row)
        && app
            .config_draft
            .as_ref()
            .is_some_and(|d| d.armed_action == Some(row))
}

/// The focused profile name, the Setup card's own selection.
fn setup_focused_name(app: &App) -> Option<crate::profile::ProfileName> {
    match app.setup_selection() {
        SetupSelection::Item(MainItemKind::Profile(i)) => {
            app.config().profiles.get(i).map(|p| p.name.clone())
        }
        _ => None,
    }
}

/// Whether the Setup card's `clear long-lived token` row is dimmed inert: the
/// [`super::config::clear_gated_facts`] formula fed the tick's facts (the
/// sidecar cache the Overview builds, the config's flags and other logins) —
/// the handler re-checks the live store before it acts, exactly as the
/// disable row's gate does.
fn clear_session_row_gated(app: &App, name: &crate::profile::ProfileName) -> bool {
    let cfg = app.config();
    let profile = cfg.find(name);
    let has_other_login = profile.is_some_and(|p| p.credentials.is_some() || p.api_key.is_some());
    let rolling_armed = profile.is_some_and(|p| p.rolling_token);
    let holds_sidecar = app.session_tokens.contains_key(name.as_str());
    let has_static_backup = crate::claude::has_static_backup(name);
    super::config::clear_gated_facts(
        has_other_login,
        rolling_armed,
        holds_sidecar,
        has_static_backup,
    )
}

/// The Setup card's hints for the row under the cursor, each `↵` naming what
/// it does on that row and none offered where the row's `↵` is inert. The
/// help modal's card `↵` row reads the same list.
pub(crate) fn setup_card_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    const ROW: (&str, &str) = ("↑↓", "row");
    let mut hints: Vec<(&'static str, &'static str)> = Vec::new();
    match app.setup_selection() {
        // The codex and pool detail panes carry their own action rows; the
        // hint derives from the row under the cursor, never a key the pane
        // does not bind.
        SetupSelection::Item(MainItemKind::Codex(_)) => {
            if app.codex_detail_armed {
                return vec![("↵", "confirm delete"), ("esc", "cancel"), ("?", "help")];
            }
            let verb = match CodexDetailRow::ALL.get(app.config_action_cursor) {
                Some(CodexDetailRow::ReLogin) => Some("log in"),
                Some(CodexDetailRow::Delete) => Some("delete"),
                None => None,
            };
            if let Some(verb) = verb {
                if walk_moves(CodexDetailRow::ALL.len()) {
                    hints.push(ROW);
                }
                hints.push(("↵", verb));
            }
        }
        SetupSelection::Item(MainItemKind::Shunt(..)) => {
            if app.pool_detail_armed {
                return vec![("↵", "confirm remove"), ("esc", "cancel"), ("?", "help")];
            }
            // kimi / unknown auth mode (or a name outside shunt's grammar):
            // no rows, facts only, nothing to walk or run.
            let rows = app.pool_detail_rows();
            let verb = match rows.get(app.config_action_cursor) {
                Some(PoolDetailRow::Refresh) => Some("refresh"),
                Some(PoolDetailRow::Remove) => Some("remove"),
                None => None,
            };
            if let Some(verb) = verb {
                if walk_moves(rows.len()) {
                    hints.push(ROW);
                }
                hints.push(("↵", verb));
            }
        }
        // Claude rows and `+ new` name each row's own keys: the `model` and
        // `effort` rows cycle on space; `alert at` steps on `+`/`-`; env rows
        // edit a value or open the add-env editor; an action row's `↵` names
        // its action.
        SetupSelection::Item(MainItemKind::Profile(_)) | SetupSelection::New => {
            if setup_delete_armed(app) {
                return vec![("↵", "confirm delete"), ("esc", "cancel"), ("?", "help")];
            }
            let rows = config_rows(app);
            if walk_moves(rows.len()) {
                hints.push(ROW);
            }
            let keys: &[(&str, &str)] = match rows.get(app.config_action_cursor).copied() {
                Some(ConfigRow::Model | ConfigRow::Effort) => {
                    &[("space", "cycle"), ("↵", "custom")]
                }
                // Each step key while its own direction moves the value; on
                // `probe every` a larger value probes less often.
                Some(row @ (ConfigRow::BellThreshold | ConfigRow::UsageProbeInterval)) => {
                    let (raise, lower) = if row == ConfigRow::BellThreshold {
                        ("raise", "lower")
                    } else {
                        ("less often", "more often")
                    };
                    if config_step_lands(app, row, true) {
                        hints.push(("+", raise));
                    }
                    if config_step_lands(app, row, false) {
                        hints.push(("-", lower));
                    }
                    &[("↵", "type")]
                }
                Some(ConfigRow::EnvEntry(_)) => &[("↵", "edit value")],
                Some(ConfigRow::EnvAdd) => &[("↵", "add env")],
                Some(ConfigRow::ModelOverrideAdd) => &[("↵", "add override")],
                Some(row @ ConfigRow::ApiKey) if minted_key_row(app, row) => &[],
                // The `type` / `family` cycle rows advance on space or ↵.
                Some(ConfigRow::Type | ConfigRow::Family) => &[("space/↵", "cycle")],
                // The disabled shunt `+ login` is inert: no ↵ hint. The one
                // predicate the row renders by, never a disk read here.
                Some(ConfigRow::Login) if app.shunt_login_disabled().is_some() => &[],
                // A proxy account's `↵` only toasts its refusal: no hint.
                Some(ConfigRow::Login)
                    if setup_focused_name(app).is_some_and(|n| login_row_refused(app, &n)) =>
                {
                    &[]
                }
                Some(ConfigRow::Login) => &[("↵", "log in")],
                Some(ConfigRow::CaptureLogin) => &[("↵", "capture")],
                Some(ConfigRow::DeleteCreds) => &[("↵", "log out")],
                Some(ConfigRow::Disabled) => match setup_disable_verb(app) {
                    Some("enable") => &[("↵", "enable")],
                    Some(_) => &[("↵", "disable")],
                    None => &[],
                },
                Some(ConfigRow::Delete) => &[("↵", "delete")],
                Some(ConfigRow::Create) => &[("↵", "create")],
                Some(ConfigRow::AutoStart | ConfigRow::UsageProbe) => &[("space/↵", "toggle")],
                Some(
                    ConfigRow::Name
                    | ConfigRow::BaseUrl
                    | ConfigRow::ApiKey
                    | ConfigRow::FableModel
                    | ConfigRow::OpusModel
                    | ConfigRow::SonnetModel
                    | ConfigRow::HaikuModel
                    | ConfigRow::SubagentModel
                    | ConfigRow::AdvisorModel,
                ) => &[("↵", "edit")],
                // The two token rows arm like the delete row: once armed the
                // footer names the confirm and its way out. The gate is read
                // BEFORE the armed branch, so an arm the gate later closes
                // drops its `↵` at once — the handler refuses at the same
                // gate, and a hidden confirm would be dead. `esc cancel` and
                // `? help` stay: esc still disarms. Unarmed, a dimmed row
                // drops only its `↵`, like every other inert row.
                Some(ConfigRow::ClearSessionToken) => {
                    let gated = setup_focused_name(app)
                        .is_some_and(|name| clear_session_row_gated(app, &name));
                    match (
                        gated,
                        setup_token_row_armed(app, ConfigRow::ClearSessionToken),
                    ) {
                        (true, true) => return vec![("esc", "cancel"), ("?", "help")],
                        (false, true) => {
                            return vec![("↵", "confirm clear"), ("esc", "cancel"), ("?", "help")];
                        }
                        (true, false) => &[],
                        (false, false) => &[("↵", "clear")],
                    }
                }
                Some(ConfigRow::RollingTokenSwitch) => {
                    let gated = setup_focused_name(app)
                        .is_some_and(|name| rolling_switch_row_gated(app, &name));
                    match (
                        gated,
                        setup_token_row_armed(app, ConfigRow::RollingTokenSwitch),
                    ) {
                        (true, true) => return vec![("esc", "cancel"), ("?", "help")],
                        (false, true) => {
                            return vec![("↵", "confirm switch"), ("esc", "cancel"), ("?", "help")];
                        }
                        (true, false) => &[],
                        (false, false) => &[("↵", "switch")],
                    }
                }
                None => &[],
            };
            hints.extend_from_slice(keys);
        }
    }
    hints.extend([("a", "actions"), ("?", "help")]);
    hints
}

/// The Config tab's hints for the row under the cursor: the three typed rows
/// cycle on space and type on `↵`; any other row's space and `↵` act only
/// while [`global_config_row_live`] holds, as its key handler checks.
fn config_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut hints = Vec::new();
    if walk_moves(GLOBAL_CONFIG_ROWS.len()) {
        hints.push(("↑↓", "row"));
    }
    match GLOBAL_CONFIG_ROWS.get(app.global_config_cursor).copied() {
        Some(
            GlobalConfigRow::RefreshInterval
            | GlobalConfigRow::ContextNudge
            | GlobalConfigRow::WeeklyThreshold,
        ) => hints.extend([("space", "cycle"), ("↵", "custom")]),
        Some(row) if global_config_row_live(app, row) => {
            hints.push(("space/↵", "cycle / toggle"));
        }
        Some(_) | None => {}
    }
    hints.extend([("a", "actions"), ("?", "help")]);
    hints
}

/// The Fallback tab's hints, each read off the predicate its key handler
/// acts on: the chain walk past one item, the reorder while either direction
/// swaps, a card row's keys while [`fallback_row_acts`] holds, each step key
/// while [`fallback_step_lands`] holds for its direction.
fn fallback_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    const ROW: (&str, &str) = ("↑↓", "row");
    let row_keys = |row: FallbackRow, keys: &[(&'static str, &'static str)]| {
        let mut hints = vec![ROW];
        if fallback_row_acts(app, row) {
            hints.extend_from_slice(keys);
        }
        hints
    };
    // A stepper row's `+` and `-` each show while their own direction moves
    // the value (a joined slot would stay while either does).
    let stepper = |row: FallbackRow| {
        let mut hints = vec![ROW];
        if fallback_step_lands(app, row, true) {
            hints.push(("+", "raise"));
        }
        if fallback_step_lands(app, row, false) {
            hints.push(("-", "lower"));
        }
        if fallback_row_acts(app, row) {
            hints.push(("↵", "type"));
        }
        hints
    };
    let mut hints = match fallback_hint(app) {
        FallbackHint::Empty => Vec::new(),
        FallbackHint::ChainMember | FallbackHint::ChainAdd => {
            let mut hints = Vec::new();
            if walk_moves(chain_items(app).len()) {
                hints.push(("↑↓", "move"));
            }
            if chain_reorder_moves(app) {
                hints.push(("⇧↑↓", "reorder"));
            }
            if chain_enter_acts(app) {
                let verb = if fallback_hint(app) == FallbackHint::ChainAdd {
                    "add"
                } else {
                    "open"
                };
                hints.push(("↵", verb));
            }
            hints
        }
        FallbackHint::DetailThreshold => stepper(FallbackRow::Threshold),
        FallbackHint::DetailWeeklyAt => stepper(FallbackRow::WeeklyAt),
        FallbackHint::DetailCheckWeekly => {
            row_keys(FallbackRow::CheckWeekly, &[("space/↵", "toggle")])
        }
        FallbackHint::DetailCheckScoped => {
            row_keys(FallbackRow::CheckScoped, &[("space/↵", "toggle")])
        }
        FallbackHint::DetailCheckCreditCap => {
            row_keys(FallbackRow::CheckCreditCap, &[("space/↵", "toggle")])
        }
        FallbackHint::DetailLastResort => {
            row_keys(FallbackRow::LastResort, &[("space/↵", "toggle")])
        }
        FallbackHint::DetailPreferred => row_keys(FallbackRow::Preferred, &[("space/↵", "toggle")]),
        // The row's own keys lead so the narrow-width trim, which drops the
        // rightmost non-essential hint first, sheds `↑↓ row` before them.
        FallbackHint::DetailPreferredDays => {
            let mut hints = Vec::new();
            if fallback_row_acts(app, FallbackRow::PreferredDays) {
                hints.extend([("space", "preset"), ("↵", "days")]);
            }
            hints.push(ROW);
            hints
        }
        FallbackHint::DetailMaxSpend => row_keys(FallbackRow::MaxSpend, &[("↵", "type")]),
        FallbackHint::DetailRemove => vec![ROW, ("↵", "remove")],
        FallbackHint::DetailRemoveArmed => {
            return vec![("↵", "confirm remove"), ("esc", "cancel"), ("?", "help")];
        }
        // The candidate count the picker's handler and the help's `↵ add`
        // read: with none left, `↵` adds nothing.
        FallbackHint::DetailAdd => {
            let candidates = chain_candidates(app).len();
            let mut hints = Vec::new();
            if walk_moves(candidates) {
                hints.push(("↑↓", "pick"));
            }
            if candidates > 0 {
                hints.push(("↵", "add"));
            }
            hints
        }
    };
    hints.extend([("a", "actions"), ("?", "help")]);
    hints
}

/// Which keys act on the Usage tab's current row and focus: the one predicate
/// set the footer's hints and the help modal's Usage rows both derive from, so
/// neither advertises a key the shown row ignores.
pub(crate) struct UsageKeyFacts {
    /// In detail focus (↑↓ scrolls, esc/← ascend).
    pub(crate) detail: bool,
    /// `↑↓` acts: the detail has somewhere to scroll, or the list a second
    /// row to walk to.
    pub(crate) arrows: bool,
    /// `↵` descends: list focus with at least one item row.
    pub(crate) enter: bool,
    /// `r` acts, and what it re-reads.
    pub(crate) r: Option<UsageRefresh>,
    /// `n` opens the note editor (a claude row with a profile to hold it).
    pub(crate) n: bool,
}

/// What a Usage `r` re-reads on the current row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageRefresh {
    /// The focused claude account's usage.
    RefreshAccount,
    /// The shunt pool listing.
    ReReadPool,
}

/// The per-row key facts: `n` acts on claude rows only, `r` where a re-read
/// exists (claude account refresh, pool listing re-read; a codex row has no
/// per-account trigger so `r` is inert there), and `↵` descends on any item
/// row. The empty roster keeps the empty state's promise.
pub(crate) fn usage_key_facts(app: &App) -> UsageKeyFacts {
    let detail = app.usage_detail_focus == StatusFocus::Detail;
    let (r, n) = match app.usage_selection() {
        SetupSelection::Item(MainItemKind::Profile(_)) if app.profile_count() > 0 => {
            (Some(UsageRefresh::RefreshAccount), true)
        }
        SetupSelection::Item(MainItemKind::Shunt(..)) => (Some(UsageRefresh::ReReadPool), false),
        SetupSelection::Item(MainItemKind::Profile(_))
        | SetupSelection::Item(MainItemKind::Codex(_))
        | SetupSelection::New => (None, false),
    };
    let items = app.setup_items().len();
    UsageKeyFacts {
        detail,
        arrows: if detail {
            detail_scrolls(app)
        } else {
            walk_moves(items)
        },
        enter: !detail && items > 0,
        r,
        n,
    }
}

/// The Usage tab's own hints, derived from [`usage_key_facts`] per row.
fn usage_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut hints: Vec<(&'static str, &'static str)> = Vec::new();
    let facts = usage_key_facts(app);
    if facts.arrows {
        hints.push(("↑↓", if facts.detail { "scroll" } else { "account" }));
    }
    if facts.enter {
        hints.push(("↵", "detail"));
    }
    match facts.r {
        Some(UsageRefresh::RefreshAccount) => hints.push(("r", "refresh account")),
        Some(UsageRefresh::ReReadPool) => hints.push(("r", "re-read pool")),
        None => {}
    }
    if facts.n {
        hints.push(("n", "note"));
    }
    hints.push(("a", "actions"));
    hints.push(("?", "help"));
    hints
}

/// The cells [`shed_to_width`] measures: each pair's key, a space, its label
/// and a 3-cell gap between pairs. The login lines measure the same rows so
/// the derived `? help` can retire where it would cost an exit key its cells;
/// the essentials alone can still overflow a floor-width row.
fn hint_row_width(hints: &[(&str, &str)]) -> usize {
    let cells: usize = hints
        .iter()
        .map(|(k, l)| k.chars().count() + 1 + l.chars().count())
        .sum();
    cells + hints.len().saturating_sub(1) * 3
}

/// Measured degradation for narrow terminals: while the row of `hints`, each
/// `key label` group 3 spaces from the next, overflows `width` cells, drop the
/// rightmost non-essential hint. Navigation the screen offers no other way
/// (`←→ tabs`, the picker's `←→ day`), discoverability (`? help`), exits
/// (`q`/`esc`) and armed confirms always survive; a field's `←→ caret` sheds,
/// since inside a text field those keys are self-evident. On a desktop-width
/// terminal everything fits and nothing changes.
fn shed_to_width(hints: &mut Vec<(&str, &str)>, width: usize) {
    while hint_row_width(hints) > width {
        match hints.iter().rposition(|h| !essential_hint(h)) {
            Some(i) => {
                hints.remove(i);
            }
            None => break,
        }
    }
}

/// A hint the row keeps under every shed: the screen's only way out or around,
/// discoverability, and armed confirms. [`draw_login`] and
/// [`draw_codex_login`] mirror this when they cap a profile name, so a long
/// name never crowds these out.
fn essential_hint(hint: &(&str, &str)) -> bool {
    let (key, label) = *hint;
    matches!(
        (key, label),
        ("←→", "tabs" | "day") | ("?" | "q" | "esc", _)
    ) || label.starts_with("confirm")
}

/// The cells the essential groups alone occupy — the width [`shed_to_width`]
/// can never go below.
fn essential_hint_width(hints: &[(&str, &str)]) -> usize {
    let kept: Vec<_> = hints.iter().copied().filter(essential_hint).collect();
    kept.iter()
        .map(|(k, l)| k.chars().count() + 1 + l.chars().count())
        .sum::<usize>()
        + kept.len().saturating_sub(1) * 3
}

/// Services tab hints under the fix grammar: `↵ <verb>` on a focused fix line
/// (dropped while an in-flight shunt action gates it), `f next fix` only while
/// the press would move focus to a fix line. `pub(super)` so the Services
/// render tests pin each focus state's whole hint list by equality.
pub(super) fn services_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    match app.services.focus {
        ServicesFocus::List => {
            let mut hints = Vec::new();
            // `↑↓`/`↵` act only while the list holds a row, the count the key
            // handler gates both on; a lone row's walk wraps onto itself.
            if walk_moves(app.services.row_count()) {
                hints.push(("↑↓", "row"));
            }
            if app.services.row_count() > 0 && app.services.enter_descends() {
                hints.push(("↵", "detail"));
            }
            hints.push(("r", "refresh"));
            if app.services.f_moves() {
                hints.push(NEXT_FIX);
            }
            hints.extend([("a", "actions"), ("?", "help")]);
            hints
        }
        ServicesFocus::Detail => services_detail_hints(app),
    }
}

/// The `f` hint under the fix grammar: the key walks, it never applies.
const NEXT_FIX: (&str, &str) = ("f", "next fix");

/// True for the five fixes an in-flight shunt action gates (a second shunt
/// press starts nothing); the plugin and herdr fixes keep working, so their
/// `↵` hint stays while a shunt action runs.
fn shunt_fix(fix: &ServiceFix) -> bool {
    matches!(
        fix,
        ServiceFix::AdoptConfig(_)
            | ServiceFix::MoveStoresIn
            | ServiceFix::AddAdminKey
            | ServiceFix::AddAdminTable
            | ServiceFix::RequireClientTokens
    )
}

/// What `space` does on the shunt card's focused line, the one predicate the
/// footer arm and the help's `space` row share, gated exactly as the key
/// handler's callees act: the `enabled` toggle while no action runs, an
/// entry while its remove is not checking, or a cycle field while its save
/// is not checking. Every other focused line's `space` does nothing.
pub(crate) enum ShuntSpaceAction {
    ToggleEnabled,
    ExpandEntry,
    CollapseEntry,
    CycleField,
}

pub(crate) fn shunt_space(app: &App) -> Option<ShuntSpaceAction> {
    match app.services.focused_shunt_focus() {
        Some(ShuntFocus::Enabled { .. }) if !app.services.shunt_action.running => {
            Some(ShuntSpaceAction::ToggleEnabled)
        }
        Some(ShuntFocus::Entry { key })
            if !app.services.saving(&ShuntSaveKey::Remove(key.clone())) =>
        {
            Some(if app.services.shunt_expanded.contains(key) {
                ShuntSpaceAction::CollapseEntry
            } else {
                ShuntSpaceAction::ExpandEntry
            })
        }
        Some(ShuntFocus::Field { key }) if !field_locked(app, key) && is_cycle_field(key) => {
            Some(ShuntSpaceAction::CycleField)
        }
        _ => None,
    }
}

/// Services detail hints, row-aware: the herdr options rows name their own
/// keys, the plugin detail walks its fixable problems, every other detail
/// scrolls.
fn services_detail_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let verb = app
        .services
        .focused_fix()
        .filter(|fix| !(app.services.shunt_action.running && shunt_fix(fix)))
        .map(fix_verb);
    let next_fix = app.services.f_moves().then_some(NEXT_FIX);
    let label = app.services.selected_check().map(|c| c.label);

    // The delegates detail walks its job list while descended. Every group
    // derives from the same count the handler steps over: ↑↓ and page keys
    // move the selection only with ≥2 jobs, so a press that cannot move
    // anything advertises nothing.
    if label == Some("delegates") {
        let mut hints = Vec::new();
        if app.services.delegates_selection_moves() {
            hints.push(("↑↓", "row"));
            hints.push(("pgup/pgdn", "read"));
        }
        hints.extend([("r", "refresh"), ("a", "actions"), ("?", "help")]);
        return hints;
    }

    // The shunt detail walks its actionable lines one at a time: `↑↓ row`
    // while a press would move focus (there are ≥2 stops), `↑↓ scroll` while
    // a press would only scroll the read-only text, and no `↑↓` at all when
    // neither applies (a fitting one-stop card). `space` acts only on the
    // focused state controls (the `enabled` toggle, an entry, a cycle field);
    // every other focused line acts on `↵`, and both drop while an action runs
    // or nothing holds focus; `f next fix` stays while the walk can move —
    // derived from the same gates the handlers read, never copied per branch.
    if label == Some("shunt") {
        let focus = app.services.focused_shunt_focus();
        let stops = app
            .services
            .selected_check()
            .map(|c| c.shunt_focus.len())
            .unwrap_or(0);
        let scroll = app
            .services
            .detail_scroll
            .min(app.services.detail_max_scroll.get());
        let mut hints = Vec::new();
        if stops > 1 {
            hints.push(("↑↓", "row"));
        } else if scroll > 0 || scroll < app.services.detail_max_scroll.get() {
            hints.push(("↑↓", "scroll"));
        }
        hints.push(("r", "refresh"));
        // Each hint derives from the same predicate its handler acts on: a fix
        // line's `↵` and the `enabled` toggle read `shunt_action.running` and
        // drop with it; a field row's edit hints read `saving(&key)` and drop
        // while its save checks (the row is busy — `↵`/`space` are inert);
        // expanding an entry and opening the check output never read either,
        // so those stay.
        match focus {
            Some(ShuntFocus::Fix { fix, .. }) if !app.services.shunt_action.running => {
                hints.push(("↵", fix_verb(fix)));
            }
            Some(ShuntFocus::CheckOutput { .. }) => hints.push(("↵", "open")),
            Some(ShuntFocus::Field { key }) if !field_locked(app, key) && !is_cycle_field(key) => {
                hints.push(("↵", "edit value"));
            }
            Some(ShuntFocus::Add { kind })
                if !app.services.saving(&ShuntSaveKey::SaveAdd(*kind)) =>
            {
                hints.push((
                    "↵",
                    if app.services.shunt_drafts.is_open(*kind) {
                        "collapse"
                    } else {
                        "add"
                    },
                ));
            }
            Some(ShuntFocus::SaveAdd { kind })
                if !app.services.saving(&ShuntSaveKey::SaveAdd(*kind)) =>
            {
                hints.push(("↵", "save"));
            }
            Some(ShuntFocus::Remove { key })
                if !app.services.saving(&ShuntSaveKey::Remove(key.clone())) =>
            {
                hints.push(("↵", "remove"));
            }
            Some(ShuntFocus::Token { key })
                if !app.services.saving(&ShuntSaveKey::Token(key.clone())) =>
            {
                hints.push(("↵", "remove"));
            }
            Some(ShuntFocus::AddToken) if !app.services.saving(&ShuntSaveKey::AddToken) => {
                hints.push(("↵", "add"));
            }
            _ => {}
        }
        // The state-control keys read [`shunt_space`], the one predicate the
        // help modal's `space` row shares: the `enabled` toggle and an entry
        // join `space` and `↵`; a cycle field splits them (its `↵` opens the
        // custom-value editor instead of stepping).
        match shunt_space(app) {
            Some(ShuntSpaceAction::ToggleEnabled) => hints.push(("space/↵", "toggle")),
            Some(ShuntSpaceAction::ExpandEntry) => hints.push(("space/↵", "expand")),
            Some(ShuntSpaceAction::CollapseEntry) => hints.push(("space/↵", "collapse")),
            Some(ShuntSpaceAction::CycleField) => {
                hints.push(("space", "cycle"));
                hints.push(("↵", "custom"));
            }
            None => {}
        }
        hints.extend(next_fix);
        hints.extend([("a", "actions"), ("?", "help")]);
        return hints;
    }

    if label != Some("herdr") {
        let problems = app
            .services
            .selected_check()
            .filter(|c| c.label == "plugin")
            .map_or(0, |c| c.problems.len());
        // The plugin walk runs over its problems and the OPTIONS row.
        let walks_problems = label == Some("plugin");
        let mut hints = Vec::new();
        // The focused row's own key leads — the option row's toggle, a
        // problem's `↵ <verb>` — so the narrow-width trim sheds the shared
        // hints before the key the shown row acts on.
        if app.services.plugin_option_acts() {
            hints.push(("space/↵", "toggle"));
        } else if services_fix_keys_act(app)
            && let Some(v) = verb
        {
            hints.push(("↵", v));
        }
        // The problem walk moves past one stop, or returns from a page's
        // hidden action focus; a scroll-only detail (a no-problem plugin card
        // included — its cursor rests on the lone option row) scrolls only
        // while its published bound leaves somewhere to go.
        if walks_problems && problems > 0 {
            if walk_moves(problems + 1) || app.services.diagnostic_focus {
                hints.push(("↑↓", "row"));
            }
        } else if detail_scrolls(app) {
            hints.push(("↑↓", "scroll"));
        }
        if walks_problems && problems > 0 && page_keys_act(app).is_some() {
            hints.push(("pgup/pgdn", "read"));
        }
        hints.push(("r", "refresh"));
        // The `f` walk acts only inside a keymap that binds it — the plugin
        // problems walk here; a scroll-only detail (any other check)
        // advertises neither, the predicate its handler routing consults.
        if services_fix_keys_act(app) {
            hints.extend(next_fix);
        }
        hints.extend([("a", "actions"), ("?", "help")]);
        return hints;
    }

    if app.services.diagnostic_focus {
        let mut hints = vec![("↑↓", "row")];
        if page_keys_act(app).is_some() {
            hints.push(("pgup/pgdn", "read"));
        }
        hints.push(("r", "refresh"));
        hints.extend(next_fix);
        hints.extend([("a", "actions"), ("?", "help")]);
        return hints;
    }

    // The herdr heal line holds focus: `↵` opens its confirm. Otherwise the
    // options rows name their own keys; `r` and `f` keep working there, `f`
    // advertised only while the check offers a fix line to walk to.
    let mut hints = match HERDR_OPTIONS.get(app.services.herdr_options_cursor) {
        _ if verb.is_some() => {
            let mut hints = vec![("↑↓", "row")];
            hints.extend(verb.map(|v| ("↵", v)));
            hints.push(("r", "refresh"));
            hints.extend(next_fix);
            hints.extend([("a", "actions"), ("?", "help")]);
            hints
        }
        // Each step key while its own direction moves the value.
        Some(HerdrOption::TagRefresh) => {
            let mut hints = vec![("↑↓", "row")];
            if herdr_tag_step(app, true).is_some() {
                hints.push(("+", "raise"));
            }
            if herdr_tag_step(app, false).is_some() {
                hints.push(("-", "lower"));
            }
            hints.extend([("↵", "type"), ("r", "refresh")]);
            hints.extend(next_fix);
            hints.extend([("a", "actions"), ("?", "help")]);
            hints
        }
        Some(HerdrOption::DelegateRowText) if herdr_config_writable(app) => {
            let mut hints = vec![("↑↓", "row"), ("space/↵", "rewrite row"), ("r", "refresh")];
            hints.extend(next_fix);
            hints.extend([("a", "actions"), ("?", "help")]);
            hints
        }
        // The inert delegate-row row advertises no activation key — it is a
        // no-op there.
        Some(HerdrOption::DelegateRowText) => {
            let mut hints = vec![("↑↓", "row"), ("r", "refresh")];
            hints.extend(next_fix);
            hints.extend([("a", "actions"), ("?", "help")]);
            hints
        }
        _ => {
            let mut hints = vec![
                ("↑↓", "row"),
                ("space/↵", "cycle / toggle"),
                ("r", "refresh"),
            ];
            hints.extend(next_fix);
            hints.extend([("a", "actions"), ("?", "help")]);
            hints
        }
    };
    if page_keys_act(app).is_some() {
        hints.insert(1, ("pgup/pgdn", "read"));
    }
    hints
}

/// Shrink a rect by `pad` columns on each side (clamped), leaving the row intact.
fn inset_x(area: Rect, pad: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(pad),
        width: area.width.saturating_sub(pad.saturating_mul(2)),
        ..area
    }
}

/// The verb and row of the save cue: `deleting` when the profile delete named
/// the row, else `saving`. The row is the one a running write is about, as the
/// card labels it: a field's label, a `+ add` kind word, an entry's displayed
/// name, `token`, `client tokens`, `shunt` for a `shunt_action` job, a pool
/// account's name, the profile being deleted, the account a rolling-token
/// switch is arming, or the account spending a refresh token. The save cue
/// names this while the exit waits or a quit is held; the priority order
/// matches the exit wait's own write predicate (save, then action, then pool,
/// then the delete, then the rolling switch, then the spend).
fn save_cue_row(app: &App) -> (&'static str, String) {
    const SAVING: &str = "saving";
    if let Some(save) = &app.services.shunt_save {
        let row = match &save.key {
            ShuntSaveKey::Field(key) => escape_control(&field_label(key)),
            ShuntSaveKey::SaveAdd(kind) => kind.word().to_string(),
            ShuntSaveKey::Remove(key) => entry_display_name(app, key),
            ShuntSaveKey::Token(name) => escape_control(name),
            ShuntSaveKey::AddToken => "token".to_string(),
            ShuntSaveKey::RequireTokens => "client tokens".to_string(),
        };
        return (SAVING, row);
    }
    if app.services.shunt_action.running {
        return (SAVING, "shunt".to_string());
    }
    if app.pool_mutation_kind == Some(PoolMutationKind::Remove)
        && let Some((_, name)) = &app.pool_mutation_row
    {
        return (SAVING, escape_control(name.as_str()));
    }
    if let Some(name) = &app.deleting {
        return ("deleting", escape_control(name.as_str()));
    }
    if let Some(name) = &app.rolling_switch {
        return (SAVING, escape_control(name.as_str()));
    }
    (
        SAVING,
        crate::usage::spending_account(&app.activity)
            .map_or_else(String::new, |name| escape_control(&name)),
    )
}

/// An entry's displayed name, as the remove row labels it: the name/id for an
/// upstream or model, the route row's whole label for a route (a route that
/// left the config since its row rendered falls back to the kind word `route`).
fn entry_display_name(app: &App, key: &ShuntEntryKey) -> String {
    match key {
        ShuntEntryKey::Upstream { name, .. } => escape_control(name),
        ShuntEntryKey::Model { id, .. } => escape_control(id),
        ShuntEntryKey::Route(_) => {
            let Some(view) = config_view(app) else {
                return "route".to_string();
            };
            match entry_for_key(view, key) {
                Some(ConfigEntryRef::Route(route)) => route_label(route, usize::MAX),
                _ => "route".to_string(),
            }
        }
    }
}

/// The info leader the save cue and its width measurement share, so the two
/// can't drift apart.
const SAVE_CUE_LEADER: &str = "i ";

/// The save cue's message: `{verb} {row}… · ⌃c ⌃c quit now`, the row name cut
/// (grapheme-safe, one `…`) so the leader and `· ⌃c ⌃c quit now` stay whole
/// down to the width floor. The `⌃c ⌃c` run is one key span. One closure holds
/// the copy, measured with an empty name to set the name's budget and rendered
/// with the cut name, so the measurement and the message cannot drift.
fn save_cue(verb: &str, row: &str, width: u16) -> String {
    let key = prose::key("\u{2303}c \u{2303}c");
    let cue = |name: &str| format!("{verb} {name}\u{2026} · {key} quit now");
    let fixed = grapheme_cells(SAVE_CUE_LEADER) + grapheme_cells(&prose::plain(&cue("")));
    let budget = (width as usize).saturating_sub(fixed);
    let mut name = prefix_in_cells(row, budget).0;
    // A cut that stops right after a `…` inside the name would double it with
    // the always-on `…`; trim a kept trailing `…` only when the cut dropped
    // something, so an uncut name keeps its own characters.
    if grapheme_cells(&name) < grapheme_cells(row) {
        name = trim_trailing_ellipsis_graphemes(&name);
    }
    cue(&name)
}

/// Render a footer alert in place of the hint bar: a semantic leader glyph
/// (`! ` warning, `i ` info) in its color, then the message in `TEXT_DIM` with
/// key runs rendered as keys, cut to fit the row (grapheme-safe, trailing `…`).
fn draw_alert(frame: &mut Frame<'_>, area: Rect, leader: &str, leader_style: Style, msg: &str) {
    let mut spans = vec![Span::styled(leader, leader_style)];
    let width = (area.width as usize).saturating_sub(grapheme_cells(leader));
    spans.extend(prose::fit(msg, width, theme::dim()));
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}

/// What esc/q/↵ do this frame for the login line's trailing hint.
#[derive(Clone, Copy)]
enum LoginKeys {
    /// The login modal is open (collapsed stage): `q` collapses it.
    Back,
    /// The login modal is collapsed: `esc` cancels the login.
    Cancel,
    /// The login modal's code field is open: `↵` submits, `esc` restores the
    /// `p  paste code` row; `q` is data.
    Paste,
    /// An open editor or a non-login modal takes the keys first: its own row.
    Owner(&'static [(&'static str, &'static str)]),
}

/// The [`LoginKeys`] a login-in-flight footer shows, shared by the claude and
/// codex lines: an open editor/modal owns esc/q first (its own row); the login
/// modal's open code field is `Paste`; collapsed it is `Back`; with no modal,
/// `Cancel`. `paste_open` names the claude modal's open code field — the codex
/// modal has no paste door, so its line passes `false`.
fn login_keys(owner: Option<KeyOwner>, app: &App, frame_area: Rect, paste_open: bool) -> LoginKeys {
    match owner.and_then(|o| owner_hints(o, app, frame_area)) {
        Some(hints) => LoginKeys::Owner(hints),
        None => match app.modals.last() {
            Some(Modal::Login) if paste_open => LoginKeys::Paste,
            Some(_) => LoginKeys::Back,
            None => LoginKeys::Cancel,
        },
    }
}

/// The root hint set a login-in-flight footer shows for its [`LoginKeys`]:
/// `paste` is the login's own paste-field hints (the claude modal's `↵ submit
/// · esc back`; the codex modal has no paste door, so none). The derived
/// `? help` joins — and retires where the row cannot hold it — in
/// [`login_row_hints`].
fn login_key_hints(
    keys: LoginKeys,
    paste: &'static [(&'static str, &'static str)],
) -> Vec<(&'static str, &'static str)> {
    match keys {
        LoginKeys::Back => &[("q", "back")],
        LoginKeys::Cancel => &[("esc", "cancel")],
        LoginKeys::Paste => paste,
        LoginKeys::Owner(hints) => hints,
    }
    .to_vec()
}

/// The login lines' shed of [`login_key_hints`]' root row: the derived
/// `? help` joins before the shed (it is essential, so the non-essential pairs
/// go first), and retires where even the kept row cannot hold it — the exit
/// keys never yield to it. No [`owner_hints`] row and none of these root rows
/// carries the pair, so the retirement can never drop an intrinsic one.
fn login_row_hints(
    mut hints: Vec<(&'static str, &'static str)>,
    owner: Option<KeyOwner>,
    app: &App,
    budget: usize,
) -> Vec<(&'static str, &'static str)> {
    insert_help(&mut hints, owner, app);
    shed_to_width(&mut hints, budget);
    if hint_row_width(&hints) > budget
        && let Some(at) = hints.iter().position(|(key, _)| *key == "?")
    {
        hints.remove(at);
    }
    hints
}

/// Login-in-progress line. Independent of `footer_alert` so key handling that
/// clears alerts never hides it. The live stage renders in the login modal;
/// this row is the collapsed view and carries the name alone. The trailing
/// hint tracks what esc/q actually do this frame: an open editor takes them
/// first (its own row); with any non-login modal open the hint is that modal's
/// own row behind the login lead; the login modal's open code field takes
/// `↵ submit   esc back`; collapsed, both cancel the login (`esc cancel`).
fn draw_login(
    frame: &mut Frame<'_>,
    area: Rect,
    session: &LoginSession,
    keys: LoginKeys,
    owner: Option<KeyOwner>,
    app: &App,
) {
    let hints = login_key_hints(keys, &[("↵", "submit"), ("esc", "back")]);
    // The name's budget is the row width minus the spinner, the `logging in ''`
    // envelope and the root essentials' cells — measured before the derived
    // `? help` joins, so the pair never costs the name a second time.
    let name_budget = (area.width as usize)
        .saturating_sub(1 + 1 + "logging in ''".len() + 3)
        .saturating_sub(essential_hint_width(&hints));
    let name = truncate(&session.name, name_budget);
    let mut spans = vec![
        Span::styled(
            format!("{} ", spinner_frame(app.tick_count)),
            theme::accent(),
        ),
        Span::styled(format!("logging in '{name}'"), theme::dim()),
    ];
    // Each hint here carries a 3-space lead, while `shed_to_width` counts a gap
    // only between groups: the name and one lead come off its budget.
    let lead = spans.iter().map(Span::width).sum::<usize>() + 3;
    let hints = login_row_hints(
        hints,
        owner,
        app,
        (area.width as usize).saturating_sub(lead),
    );
    for (key, action) in hints {
        spans.push(Span::styled(format!("   {key} "), theme::accent().bold()));
        spans.push(Span::styled(action, theme::dim()));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}

/// Codex-login-in-progress line, the [`draw_login`] shape without the paste
/// door: the name, plus the hint that tracks what esc/q do this frame.
fn draw_codex_login(
    frame: &mut Frame<'_>,
    area: Rect,
    session: &CodexLoginSession,
    keys: LoginKeys,
    owner: Option<KeyOwner>,
    app: &App,
) {
    let hints = login_key_hints(keys, &[]);
    // The name's budget is the [`draw_login`] rule: the row width minus the
    // spinner, the `logging in ''` envelope and the root essentials' cells —
    // measured before the derived `? help` joins, so the pair never costs the
    // name a second time.
    let name_budget = (area.width as usize)
        .saturating_sub(1 + 1 + "logging in ''".len() + 3)
        .saturating_sub(essential_hint_width(&hints));
    let name = truncate(&session.name, name_budget);
    let mut spans = vec![
        Span::styled(
            format!("{} ", spinner_frame(app.tick_count)),
            theme::accent(),
        ),
        Span::styled(format!("logging in '{name}'"), theme::dim()),
    ];
    // Each hint here carries a 3-space lead, while `shed_to_width` counts a gap
    // only between groups: the name and one lead come off its budget.
    let lead = spans.iter().map(Span::width).sum::<usize>() + 3;
    let hints = login_row_hints(
        hints,
        owner,
        app,
        (area.width as usize).saturating_sub(lead),
    );
    for (key, action) in hints {
        spans.push(Span::styled(format!("   {key} "), theme::accent().bold()));
        spans.push(Span::styled(action, theme::dim()));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_footer.rs"]
mod tests;
