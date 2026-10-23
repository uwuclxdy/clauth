//! Bottom strip: key hints, or a footer alert in place when one is active.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, CodexDetailRow, CodexLoginSession, ConfigEntryRef, ConfigFocus, ConfigRow, FallbackHint,
    FooterAlert, GLOBAL_CONFIG_ROWS, GlobalConfigRow, HERDR_OPTIONS, HerdrOption, KeyOwner,
    LoginSession, LossWarningTarget, MainItemKind, Modal, PaneState, PoolDetailRow,
    PoolMutationKind, ScrollStamp, ServiceFix, ServicesFocus, SetupSelection, ShuntEntryKey,
    ShuntFieldEdit, ShuntFieldKey, ShuntFocus, ShuntSaveKey, StatusFocus, Tab, TokenView,
    build_action_menu, config_rows, config_view, entry_for_key, escape_control, fallback_hint,
    field_locked, fix_verb, has_sub_focus, herdr_config_writable, is_cycle_field, keyboard_owner,
    pane_state, token_model_count,
};
use super::super::theme;
use super::format::{grapheme_cells, prefix_in_cells, spinner_frame, truncate};
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
/// ↵ / space commit the highlighted one, `q` backs out.
const BUTTON_KEYS: &[(&str, &str)] = &[("←→", "pick"), ("↵", "choose"), ("q", "back")];

/// The loss warning's three buttons: ←→ steps and tab wraps between them, ↵ /
/// space choose the highlighted one, `esc` and `q` both `go back`.
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
        let msg = save_cue(&saving_row_name(app), area.width);
        draw_alert(frame, area, "i ", theme::info(), &msg);
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
        draw_login(frame, area, session, keys, app.tick_count);
        return;
    }

    // A codex browser re-login in flight owns the footer the same way: the
    // modal is the live view, this row is the collapsed one carrying the name.
    // As on the claude login line, any open non-login modal owns esc/q first,
    // so its own row rides behind the codex lead; collapsed, esc cancels.
    if let Some(session) = &app.codex_login {
        let keys = login_keys(owner, app, frame_area, false);
        draw_codex_login(frame, area, session, keys, app.tick_count);
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
                if !owner.claims(app, KeyCode::Char('?')) {
                    let at = hints
                        .iter()
                        .position(|(key, _)| *key == "q")
                        .unwrap_or(hints.len());
                    hints.insert(at, ("?", "help"));
                }
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
    if build_action_menu(app).items.is_empty() {
        hints.retain(|(key, _)| *key != "a");
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
            Some(Modal::ActionMenu(_)) => Some(LIST_KEYS),
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

    let tail: &[(&str, &str)] = match app.tab {
        Tab::Overview => &[
            ("⇧↑↓", "reorder"),
            ("a", "actions"),
            ("c", "harness"),
            ("?", "help"),
        ],
        Tab::Usage => &[], // handled above: its hints derive per row
        Tab::Tokens => match app.token_view {
            TokenView::Dashboard if token_model_count(app) == 0 => &[
                ("r", "reload"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
            TokenView::Dashboard => &[
                ("↵", "models"),
                ("r", "reload"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
            TokenView::Models if app.model_detail_focus == StatusFocus::Detail => &[
                ("↑↓", "scroll"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
            TokenView::Models if token_model_count(app) == 0 => &[
                ("↑↓", "model"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
            TokenView::Models => &[
                ("↑↓", "model"),
                ("↵", "detail"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
        },
        Tab::Setup => match app.config_focus {
            ConfigFocus::Profiles => &[
                ("↑↓", "account"),
                ("↵", "configure"),
                ("n", "new"),
                ("a", "actions"),
                ("?", "help"),
            ],
            ConfigFocus::Actions => match app.setup_selection() {
                // The codex and pool detail panes carry their own action rows;
                // the hint derives from the row under the cursor, never a dead
                // `↵ edit / toggle` or a key the pane does not bind.
                SetupSelection::Item(MainItemKind::Codex(_)) => {
                    if app.codex_detail_armed {
                        &[("↵", "confirm delete"), ("esc", "cancel"), ("?", "help")]
                    } else {
                        match CodexDetailRow::ALL.get(app.config_action_cursor) {
                            Some(CodexDetailRow::ReLogin) => &[
                                ("↑↓", "row"),
                                ("↵", "re-login"),
                                ("a", "actions"),
                                ("?", "help"),
                            ],
                            Some(CodexDetailRow::Delete) => &[
                                ("↑↓", "row"),
                                ("↵", "delete"),
                                ("a", "actions"),
                                ("?", "help"),
                            ],
                            None => &[("a", "actions"), ("?", "help")],
                        }
                    }
                }
                SetupSelection::Item(MainItemKind::Shunt(..)) => {
                    if app.pool_detail_armed {
                        &[("↵", "confirm remove"), ("esc", "cancel"), ("?", "help")]
                    } else {
                        let rows = app.pool_detail_rows();
                        if rows.is_empty() {
                            // kimi / unknown auth mode (or a name outside
                            // shunt's grammar): facts only, nothing to walk or
                            // run — advertise no row key.
                            &[("a", "actions"), ("?", "help")]
                        } else {
                            match rows.get(app.config_action_cursor) {
                                Some(PoolDetailRow::Refresh) => &[
                                    ("↑↓", "row"),
                                    ("↵", "refresh"),
                                    ("a", "actions"),
                                    ("?", "help"),
                                ],
                                Some(PoolDetailRow::Remove) => &[
                                    ("↑↓", "row"),
                                    ("↵", "remove"),
                                    ("a", "actions"),
                                    ("?", "help"),
                                ],
                                None => &[("a", "actions"), ("?", "help")],
                            }
                        }
                    }
                }
                // Claude rows and `+ new` keep the row-aware claude arm:
                // the `model` row cycles on space; `alert at` steps on
                // `+`/`-`; env rows edit a value or open the add-env editor.
                SetupSelection::Item(MainItemKind::Profile(_)) | SetupSelection::New => {
                    match config_rows(app).get(app.config_action_cursor) {
                        Some(ConfigRow::Model) => &[
                            ("↑↓", "row"),
                            ("space", "cycle"),
                            ("↵", "custom"),
                            ("a", "actions"),
                            ("?", "help"),
                        ],
                        Some(ConfigRow::BellThreshold) => &[
                            ("↑↓", "row"),
                            ("+", "raise"),
                            ("-", "lower"),
                            ("↵", "type"),
                            ("a", "actions"),
                            ("?", "help"),
                        ],
                        // `+`/`-` only step a running probe, so they are only
                        // offered while it runs; off, the row is a plain toggle.
                        Some(ConfigRow::UsageProbe) if app.editing_usage_probe_on() => &[
                            ("↑↓", "row"),
                            ("↵", "toggle"),
                            ("+", "less often"),
                            ("-", "more often"),
                            ("a", "actions"),
                            ("?", "help"),
                        ],
                        Some(ConfigRow::EnvEntry(_)) => &[
                            ("↑↓", "row"),
                            ("↵", "edit value"),
                            ("a", "actions"),
                            ("?", "help"),
                        ],
                        Some(ConfigRow::EnvAdd) => &[
                            ("↑↓", "row"),
                            ("↵", "add env"),
                            ("a", "actions"),
                            ("?", "help"),
                        ],
                        Some(ConfigRow::ModelOverrideAdd) => &[
                            ("↑↓", "row"),
                            ("↵", "add override"),
                            ("a", "actions"),
                            ("?", "help"),
                        ],
                        // The mint-seeded api key row is a Disabled row (focusable
                        // but inert), so the footer drops its edit hint.
                        Some(ConfigRow::ApiKey)
                            if app.config_draft.as_ref().is_some_and(|d| d.mint_armed()) =>
                        {
                            &[("↑↓", "row"), ("a", "actions"), ("?", "help")]
                        }
                        // The `type` / `family` cycle rows advance on space or ↵.
                        Some(ConfigRow::Type) | Some(ConfigRow::Family) => &[
                            ("↑↓", "row"),
                            ("space/↵", "cycle"),
                            ("a", "actions"),
                            ("?", "help"),
                        ],
                        // The disabled shunt `+ login` is inert: no ↵ hint. The
                        // one predicate the row renders by, never a disk read
                        // here.
                        Some(ConfigRow::Login) if app.shunt_login_disabled().is_some() => {
                            &[("↑↓", "row"), ("a", "actions"), ("?", "help")]
                        }
                        _ => &[
                            ("↑↓", "row"),
                            ("↵", "edit / toggle"),
                            ("a", "actions"),
                            ("?", "help"),
                        ],
                    }
                }
            },
        },
        Tab::Config => {
            if GLOBAL_CONFIG_ROWS
                .get(app.global_config_cursor)
                .is_some_and(|r| {
                    matches!(
                        r,
                        GlobalConfigRow::RefreshInterval
                            | GlobalConfigRow::ContextNudge
                            | GlobalConfigRow::WeeklyThreshold
                    )
                })
            {
                &[
                    ("↑↓", "row"),
                    ("space", "cycle"),
                    ("↵", "custom"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("space/↵", "cycle / toggle"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            }
        }
        Tab::Status => match app.status.focus {
            StatusFocus::List => &[
                ("↑↓", "incident"),
                ("↵", "open"),
                ("r", "refresh"),
                ("a", "actions"),
                ("?", "help"),
            ],
            StatusFocus::Detail => &[("↑↓", "scroll"), ("a", "actions"), ("?", "help")],
        },
        Tab::Services => &[], // handled above: its hints carry a computed verb
        Tab::Fallback => match fallback_hint(app) {
            FallbackHint::Empty => &[("a", "actions"), ("?", "help")],
            FallbackHint::ChainMember => &[
                ("↑↓", "move"),
                ("⇧↑↓", "reorder"),
                ("↵", "open"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::ChainAdd => &[
                ("↑↓", "move"),
                ("↵", "add"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailThreshold => &[
                ("↑↓", "row"),
                ("+", "raise"),
                ("-", "lower"),
                ("↵", "type"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailWeeklyAt => &[
                ("↑↓", "row"),
                ("+", "raise"),
                ("-", "lower"),
                ("↵", "type"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailCheckWeekly
            | FallbackHint::DetailCheckScoped
            | FallbackHint::DetailCheckCreditCap
            | FallbackHint::DetailLastResort
            | FallbackHint::DetailPreferred => &[
                ("↑↓", "row"),
                ("space/↵", "toggle"),
                ("a", "actions"),
                ("?", "help"),
            ],
            // The row's own keys lead so the narrow-width trim, which drops the
            // rightmost non-essential hint first, sheds `↑↓ row` before them.
            FallbackHint::DetailPreferredDays => &[
                ("space", "preset"),
                ("↵", "days"),
                ("↑↓", "row"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailMaxSpend => &[
                ("↑↓", "row"),
                ("↵", "type"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailRemove => &[
                ("↑↓", "row"),
                ("↵", "remove"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailRemoveArmed => {
                &[("↵", "confirm remove"), ("esc", "cancel"), ("?", "help")]
            }
            FallbackHint::DetailAdd => &[
                ("↑↓", "pick"),
                ("↵", "add"),
                ("a", "actions"),
                ("?", "help"),
            ],
        },
    };

    let mut hints = tail.to_vec();
    // An armed 2-step's way out is its `esc cancel`: `q` ascends out of the card
    // the same way there, so `q back` would be a second name for it. The codex
    // and pool details arm the same way, so they skip `q` too.
    let setup_armed = app.tab == Tab::Setup
        && app.config_focus == ConfigFocus::Actions
        && (app.codex_detail_armed || app.pool_detail_armed);
    if !(setup_armed
        || (app.tab == Tab::Fallback && fallback_hint(app) == FallbackHint::DetailRemoveArmed))
    {
        hints.push(("q", q_label));
    }
    hints
}

/// Which keys act on the Usage tab's current row and focus: the one predicate
/// set the footer's hints and the help modal's Usage rows both derive from, so
/// neither advertises a key the shown row ignores.
pub(crate) struct UsageKeyFacts {
    /// In detail focus (↑↓ scrolls, esc/← ascend).
    pub(crate) detail: bool,
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
    UsageKeyFacts {
        detail,
        enter: !detail && !app.setup_items().is_empty(),
        r,
        n,
    }
}

/// The Usage tab's own hints, derived from [`usage_key_facts`] per row.
fn usage_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut hints: Vec<(&'static str, &'static str)> = Vec::new();
    let facts = usage_key_facts(app);
    if facts.detail {
        hints.push(("↑↓", "scroll"));
    } else {
        hints.push(("↑↓", "account"));
        if facts.enter {
            hints.push(("↵", "detail"));
        }
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

/// Measured degradation for narrow terminals: while the row of `hints`, each
/// `key label` group 3 spaces from the next, overflows `width` cells, drop the
/// rightmost non-essential hint. Navigation the screen offers no other way
/// (`←→ tabs`, the picker's `←→ day`), discoverability (`? help`), exits
/// (`q`/`esc`) and armed confirms always survive; a field's `←→ caret` sheds,
/// since inside a text field those keys are self-evident. On a desktop-width
/// terminal everything fits and nothing changes.
fn shed_to_width(hints: &mut Vec<(&str, &str)>, width: usize) {
    let row_width = |hints: &[(&str, &str)]| -> usize {
        let cells: usize = hints
            .iter()
            .map(|(k, l)| k.chars().count() + 1 + l.chars().count())
            .sum();
        cells + hints.len().saturating_sub(1) * 3
    };
    while row_width(hints) > width {
        match hints.iter().rposition(|h| !essential_hint(h)) {
            Some(i) => {
                hints.remove(i);
            }
            None => break,
        }
    }
}

/// A hint the row keeps under every shed: the screen's only way out or around,
/// discoverability, and armed confirms. [`draw_login`] mirrors this when it
/// caps a profile name, so a long name never crowds these out.
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
            let mut hints = vec![("↑↓", "row")];
            // `↵` descends only where the handler descends (a job-less
            // delegates row advertises no inert `↵`) — one shared predicate.
            if app.services.enter_descends() {
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
    // neither applies (a fitting one-stop card). `space/↵` act only
    // on the focused line, and drop while an action runs or nothing holds
    // focus; `f next fix` stays while the walk can move — derived from the
    // same gates the handlers read, never copied per branch.
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
            Some(ShuntFocus::Enabled { .. }) if !app.services.shunt_action.running => {
                hints.push(("space/↵", "toggle"));
            }
            Some(ShuntFocus::CheckOutput { .. }) => hints.push(("space/↵", "open")),
            Some(ShuntFocus::Entry { key })
                if !app.services.saving(&ShuntSaveKey::Remove(key.clone())) =>
            {
                hints.push((
                    "space/↵",
                    if app.services.shunt_expanded.contains(key) {
                        "collapse"
                    } else {
                        "expand"
                    },
                ));
            }
            Some(ShuntFocus::Field { key }) if !field_locked(app, key) => {
                if is_cycle_field(key) {
                    hints.push(("space", "cycle"));
                    hints.push(("↵", "custom"));
                } else {
                    hints.push(("↵", "edit value"));
                }
            }
            Some(ShuntFocus::Add { kind })
                if !app.services.saving(&ShuntSaveKey::SaveAdd(*kind)) =>
            {
                hints.push((
                    "space/↵",
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
        hints.extend(next_fix);
        hints.extend([("a", "actions"), ("?", "help")]);
        return hints;
    }

    if label != Some("herdr") {
        let walks_problems = label == Some("plugin")
            && app
                .services
                .selected_check()
                .is_some_and(|c| !c.problems.is_empty());
        let mut hints = if walks_problems {
            vec![("↑↓", "problem")]
        } else {
            vec![("↑↓", "scroll")]
        };
        if walks_problems && app.services.detail_max_scroll.get() > 0 {
            hints.push(("pgup/pgdn", "read"));
        }
        hints.push(("r", "refresh"));
        if let Some(v) = verb {
            hints.push(("↵", v));
        }
        hints.extend(next_fix);
        hints.extend([("a", "actions"), ("?", "help")]);
        return hints;
    }

    if app.services.diagnostic_focus {
        let mut hints = vec![("↑↓", "row")];
        if app.services.detail_max_scroll.get() > 0 {
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
        Some(HerdrOption::TagRefresh) => {
            let mut hints = vec![
                ("↑↓", "row"),
                ("+", "raise"),
                ("-", "lower"),
                ("↵", "type"),
                ("r", "refresh"),
            ];
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
    if app.services.detail_max_scroll.get() > 0 {
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

/// The row a running write is about, as the card labels it: a field's label, a
/// `+ add` kind word, an entry's displayed name, `token`, `client tokens`,
/// `shunt` for a `shunt_action` job, or a pool account's name. The save cue
/// names this while the exit waits or a quit is held; the priority order
/// matches the exit wait's own write predicate (save, then action, then pool).
fn saving_row_name(app: &App) -> String {
    if let Some(save) = &app.services.shunt_save {
        return match &save.key {
            ShuntSaveKey::Field(key) => escape_control(&field_label(key)),
            ShuntSaveKey::SaveAdd(kind) => kind.word().to_string(),
            ShuntSaveKey::Remove(key) => entry_display_name(app, key),
            ShuntSaveKey::Token(name) => escape_control(name),
            ShuntSaveKey::AddToken => "token".to_string(),
            ShuntSaveKey::RequireTokens => "client tokens".to_string(),
        };
    }
    if app.services.shunt_action.running {
        return "shunt".to_string();
    }
    if app.pool_mutation_kind == Some(PoolMutationKind::Remove)
        && let Some((_, name)) = &app.pool_mutation_row
    {
        return escape_control(name.as_str());
    }
    String::new()
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

/// The save cue's message: `saving {row}… · ⌃c ⌃c quit now`, the row name cut
/// (grapheme-safe, one `…`) so the leader and `· ⌃c ⌃c quit now` stay whole
/// down to the width floor. The `⌃c ⌃c` run is one key span. The name's budget
/// is the cells the copy itself spends around it — the `i ` leader plus the
/// same message with an empty name, measured — so a copy edit re-derives the
/// cut instead of drifting a hand-counted fixed.
fn save_cue(row: &str, width: u16) -> String {
    let key = prose::key("\u{2303}c \u{2303}c");
    let fixed = grapheme_cells("i ")
        + grapheme_cells(&prose::plain(&format!("saving \u{2026} · {key} quit now")));
    let budget = (width as usize).saturating_sub(fixed);
    let mut name = prefix_in_cells(row, budget).0;
    // A cut that stops right after a `…` inside the name would double it with
    // the always-on `…`; trim a kept trailing `…` only when the cut dropped
    // something, so an uncut name keeps its own characters.
    if grapheme_cells(&name) < grapheme_cells(row) {
        name = name.trim_end_matches('\u{2026}').to_string();
    }
    format!("saving {name}\u{2026} · {key} quit now")
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

/// The hint set a login-in-flight footer shows for its [`LoginKeys`]. `paste`
/// is the login's own paste-field hints (the claude modal's `↵ submit · esc
/// back`; the codex modal has no paste door, so none).
fn login_key_hints(
    keys: LoginKeys,
    paste: &'static [(&'static str, &'static str)],
) -> &'static [(&'static str, &'static str)] {
    match keys {
        LoginKeys::Back => &[("q", "back")],
        LoginKeys::Cancel => &[("esc", "cancel")],
        LoginKeys::Paste => paste,
        LoginKeys::Owner(hints) => hints,
    }
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
    tick: u64,
) {
    let hints = login_key_hints(keys, &[("↵", "submit"), ("esc", "back")]);
    // The name's budget is the row width minus the spinner, the `logging in ''`
    // envelope and the essential hints' cells, so a long profile name never
    // crowds out the one key that leaves the modal.
    let name_budget = (area.width as usize)
        .saturating_sub(1 + 1 + "logging in ''".len() + 3)
        .saturating_sub(essential_hint_width(hints));
    let name = truncate(&session.name, name_budget);
    let mut spans = vec![
        Span::styled(format!("{} ", spinner_frame(tick)), theme::accent()),
        Span::styled(format!("logging in '{name}'"), theme::dim()),
    ];
    // Each hint here carries a 3-space lead, while `shed_to_width` counts a gap
    // only between groups: the name and one lead come off its budget.
    let lead = spans.iter().map(Span::width).sum::<usize>() + 3;
    let mut hints = hints.to_vec();
    shed_to_width(&mut hints, (area.width as usize).saturating_sub(lead));
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
    tick: u64,
) {
    let hints = login_key_hints(keys, &[]);
    let mut spans = vec![
        Span::styled(format!("{} ", spinner_frame(tick)), theme::accent()),
        Span::styled(format!("logging in '{}'", session.name), theme::dim()),
    ];
    let lead = spans.iter().map(Span::width).sum::<usize>() + 3;
    let mut hints = hints.to_vec();
    shed_to_width(&mut hints, (area.width as usize).saturating_sub(lead));
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
