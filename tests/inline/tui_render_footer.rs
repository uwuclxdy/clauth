//! Footer hint-bar pins.

use super::*;
use crate::gateway_admin::{PoolFamily, ShuntAccountName};
use crate::profile::{AppConfig, AppState};
use crate::tui::app::{ModelFieldKind, PoolMutationKind, ShuntAddKind, ShuntEntryKey, ShuntSave};

fn bare_app() -> App {
    App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    })
}

/// The frame the derivation test reads a scroll stamp against: 80x24, matching
/// [`bare_app`]'s modal-open generation 0 — the same size the render pins draw
/// at, so a `prep` that fakes a bound also fakes the stamp the footer would
/// have read had that frame really drawn.
fn stamp_area() -> ratatui::layout::Rect {
    ratatui::layout::Rect::new(0, 0, 80, 24)
}

/// The stamp a faked bound carries in the derivation fixtures: [`stamp_area`]
/// plus `bare_app`'s generation 0.
fn fresh_stamp() -> ScrollStamp {
    ScrollStamp {
        width: 80,
        height: 24,
        generation: 0,
    }
}

/// The Usage tab advertises the note editor key while an account exists, and
/// drops it on an empty roster — there `n` still starts a new account (the
/// hint derives from the key's behavior on this frame).
#[test]
fn the_usage_hints_advertise_the_note_key() {
    use crate::profile::Profile;
    use std::collections::BTreeMap;
    let _home = crate::testutil::HomeSandbox::new();
    let profile = Profile {
        name: "alice".into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        usage_probe: false,
        usage_probe_interval_ms: crate::profile::DEFAULT_USAGE_PROBE_INTERVAL_MS,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        preferred_days: Vec::new(),
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        check_credit_cap: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        has_long_lived_sidecar: false,
        provider: None,
    };
    let with_account = App::new(crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    });
    let mut app = with_account;
    app.tab = Tab::Usage;
    let hints = tab_hints(&app);
    assert!(
        hints.contains(&("n", "note")),
        "usage hints must name the note key, got {hints:?}"
    );

    let mut empty = bare_app();
    empty.tab = Tab::Usage;
    let hints = tab_hints(&empty);
    assert!(
        hints.iter().all(|(k, _)| *k != "n"),
        "an empty roster keeps n = new account off the hint bar, got {hints:?}"
    );
}

/// The Usage `r` hint derives from the row: `re-read pool` on a pool row, no
/// `r` on a codex row (F7-PL4).
#[test]
fn the_usage_r_hint_derives_from_the_row() {
    use crate::gateway_admin::{PoolAccount, PoolAuth, PoolProvider};
    use crate::profile::ProfileName;
    use crate::tui::app::ShuntPool;
    use ratatui::crossterm::event::KeyCode;

    let _home = crate::testutil::HomeSandbox::new();
    let pool_account = |name: &str| PoolAccount {
        name: name.to_string(),
        available: true,
        near_quota: false,
        cooldown_secs_remaining: None,
        needs_relogin: false,
        paused: false,
        disabled: false,
        utilization_5h: None,
        reset_5h: None,
        utilization_7d: None,
        reset_7d: None,
        utilization_7d_oi: None,
        reset_7d_oi: None,
        plan: None,
        status: None,
    };
    let with_claude = || {
        App::new(AppConfig {
            state: AppState {
                profiles: vec![ProfileName::from("cl1")],
                ..AppState::default()
            },
            profiles: vec![crate::testutil::blank_profile(&ProfileName::from("cl1"))],
        })
    };

    let mut pool = with_claude();
    pool.shunt_pool = ShuntPool::Read(vec![PoolProvider {
        name: "claude-main".to_string(),
        auth: PoolAuth::Claude,
        accounts: vec![pool_account("alpha")],
    }]);
    pool.tab = Tab::Usage;
    crate::tui::app::handle_key(&mut pool, crate::testutil::key(KeyCode::Down));
    assert!(
        tab_hints(&pool).contains(&("r", "re-read pool")),
        "a pool row advertises `r re-read pool`: {:?}",
        tab_hints(&pool)
    );

    crate::testutil::write_codex_roster(&["cx1"]);
    let mut codex = with_claude();
    codex.tab = Tab::Usage;
    crate::tui::app::handle_key(&mut codex, crate::testutil::key(KeyCode::Down));
    assert!(
        !tab_hints(&codex).iter().any(|(k, _)| *k == "r"),
        "a codex row advertises no `r`: {:?}",
        tab_hints(&codex)
    );
}

/// On the Setup tab's `alert at` row the bar names the stepper keys, the same
/// pair the Fallback card's threshold row names.
#[test]
fn the_setup_alert_row_advertises_its_stepper_keys() {
    use crate::tui::app::{ConfigFocus, ConfigRow, config_rows};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![crate::profile::Profile::new("a".to_string(), None, None)],
    });
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    app.profile_cursor.set(0);
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::BellThreshold)
        .expect("the alert row is listed");
    assert_eq!(
        tab_hints(&app),
        [
            ("↑↓", "row"),
            ("+", "raise"),
            ("-", "lower"),
            ("↵", "type"),
            ("a", "actions"),
            ("?", "help"),
            ("q", "back"),
        ]
    );
}

/// A draft field's typed editor reads `↵ done   ←→ caret   esc revert`, a
/// draft cycle row's pending pick `↵ done   esc revert` — the Setup `+ new`
/// form's `done` word, `esc revert` kept because the draft editor's `esc`
/// restores the saved value instead of dropping it.
#[test]
fn the_draft_editors_read_done_not_save() {
    use crate::tui::app::{
        InputState, ShuntAddKind, ShuntDraftField, ShuntFieldEdit, ShuntFieldEditState,
        ShuntFieldKey, UpstreamFieldKind,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let draft_key = ShuntFieldKey::Draft {
        kind: ShuntAddKind::Upstream,
        field: ShuntDraftField::Upstream(UpstreamFieldKind::Name),
    };

    let mut typed = bare_app();
    typed.services.shunt_field_edit = Some(ShuntFieldEditState {
        key: draft_key.clone(),
        edit: ShuntFieldEdit::Typed {
            input: InputState::new(""),
            saved: None,
        },
        refusal: None,
    });
    assert_eq!(
        owner_hints(KeyOwner::ShuntField, &typed, stamp_area()),
        Some(&[("↵", "done"), ("←→", "caret"), ("esc", "revert")][..]),
        "a draft field's typed editor reads `↵ done`, never `↵ save`"
    );

    let mut pick = bare_app();
    pick.services.shunt_field_edit = Some(ShuntFieldEditState {
        key: draft_key,
        edit: ShuntFieldEdit::Pick {
            saved: None,
            picked: "api_key".into(),
        },
        refusal: None,
    });
    assert_eq!(
        owner_hints(KeyOwner::ShuntField, &pick, stamp_area()),
        Some(&[("↵", "done"), ("esc", "revert")][..]),
        "a draft cycle row's pending pick reads `↵ done   esc revert`"
    );
}

/// The re-bind confirm's footer row is pinned at both sizes: the ruled
/// `↑↓ row   space toggle   ↵ choose   q not now` copy (an account list the
/// cursor walks) plus the derived `? help` before `q`; a narrow terminal sheds
/// `↵ choose` first.
#[test]
fn the_rebind_confirm_footer_row_is_pinned() {
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();
    let profiles = vec![
        Profile::new(
            "D1".to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            None,
        ),
        Profile::new(
            "D2".to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            None,
        ),
    ];
    let app = App::new(AppConfig {
        state: AppState {
            profiles: profiles.iter().map(|p| p.name.clone()).collect(),
            ..AppState::default()
        },
        profiles,
    });
    assert!(
        matches!(app.modals.first(), Some(Modal::Rebind(_))),
        "the re-bind confirm is open"
    );

    for (w, h, expected) in [
        (
            80u16,
            40u16,
            "↑↓ row   space toggle   ↵ choose   ? help   q not now",
        ),
        (45u16, 24u16, "↑↓ row   space toggle   ? help   q not now"),
    ] {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("backend");
        term.draw(|f| crate::tui::render::draw(f, &app))
            .expect("draw");
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let footer = rows.last().expect("the footer row").trim();
        assert_eq!(footer, expected, "the rebind footer row at {w}x{h}");
    }
}

// ── modal footer derivation ──────────────────────────────────────────────────

/// The hint pairs one modal's footer derives from.
type HintRow = &'static [(&'static str, &'static str)];

/// One `Modal` variant state whose footer `owner_hints` derives. `row` is the
/// settled 80-col footer row (`None` for a derivation-only frame whose content
/// cannot produce it at the two pinned sizes); `row_45` the 45-col row when it
/// sheds. `prep` sets the frame state the `hints` describe — the scroll bound,
/// a built-in pick — so the derivation drive is deterministic without a draw.
struct ModalCase {
    name: &'static str,
    modal: Modal,
    row: Option<&'static str>,
    row_45: Option<&'static str>,
    hints: HintRow,
    prep: fn(&mut App),
}

/// Draw `app` twice at `w`x`h` and return the buffer's rows.
fn settled_rows(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("backend");
    term.draw(|f| crate::tui::render::draw(f, app))
        .expect("draw");
    term.draw(|f| crate::tui::render::draw(f, app))
        .expect("draw");
    crate::testutil::buffer_rows(term.backend().buffer())
}

/// Draw `app` twice at `w`x`h` and return the settled footer row (trimmed).
fn settled_footer(app: &App, w: u16, h: u16) -> String {
    settled_rows(app, w, h)
        .last()
        .expect("the footer row")
        .trim()
        .to_string()
}

fn modal_cases() -> Vec<ModalCase> {
    use crate::actions::{AnchorAction, CaptureSnapshot};
    use crate::presets::{Preset, RebindBlock, RowMove};
    use crate::tui::app::{
        ActionMenuAction, ActionMenuState, CaptureNameForm, ConfirmAction, ConfirmScroll,
        ConfirmState, Dirty, DivergenceForm, DivergenceTargetForm, EnvCollisionForm, InputState,
        LossWarningButton, LossWarningDrop, LossWarningState, LossWarningTarget, NamePromptAction,
        NamePromptForm, PresetApplyButton, PresetApplyForm, PresetPickerForm, PresetTarget,
        RebindForm, TokenPickerForm,
    };
    use std::cell::{Cell, RefCell};

    let preset = Preset {
        name: "p".into(),
        base_url: None,
        models: Default::default(),
        builtin: false,
    };
    let builtin = Preset {
        name: "qwen".into(),
        base_url: None,
        models: Default::default(),
        builtin: true,
    };
    let collision = |reviewing_overwrite: bool| {
        Modal::EnvCollision(EnvCollisionForm {
            profile: "a".into(),
            key: "K".into(),
            reason: "set by the base url field".into(),
            existing_idx: None,
            cursor: 0,
            scroll: Cell::new(0),
            reviewing_overwrite,
            confirm_overwrite: false,
            max_scroll: Cell::new(0),
            max_scroll_stamp: Cell::new(ScrollStamp::default()),
            viewport: Cell::new(0),
            overwrite_needs_review: Cell::new(false),
        })
    };
    let confirm = |detail: Option<String>| {
        Modal::Confirm(ConfirmState {
            message: "rotate 'x'?".into(),
            detail,
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::RotateOne("x".into()),
        })
    };
    let preset_apply = |message: String, leaves: Vec<&'static str>| {
        Modal::PresetApply(PresetApplyForm {
            target: PresetTarget::NewAccount,
            preset: preset.clone(),
            message,
            replaces: vec!["model"],
            leaves,
            focus: PresetApplyButton::Keep,
            scroll: ConfirmScroll::default(),
        })
    };
    vec![
        ModalCase {
            name: "help",
            modal: Modal::Help,
            row: Some("↑↓ scroll   q back"),
            row_45: None,
            hints: &[("↑↓", "scroll"), ("q", "back")],
            prep: |a| {
                a.help_max_scroll.set(1);
                a.help_max_scroll_stamp.set(fresh_stamp());
            },
        },
        ModalCase {
            name: "help fit",
            modal: Modal::Help,
            row: None,
            row_45: None,
            hints: &[("q", "back")],
            prep: |a| a.help_max_scroll.set(0),
        },
        ModalCase {
            name: "check output",
            modal: Modal::CheckOutput("masked stderr line\n".repeat(60)),
            row: Some("↑↓ scroll   q back"),
            row_45: None,
            hints: &[("↑↓", "scroll"), ("q", "back")],
            prep: |a| {
                a.check_output_max_scroll.set(1);
                a.check_output_max_scroll_stamp.set(fresh_stamp());
            },
        },
        ModalCase {
            name: "check output fit",
            modal: Modal::CheckOutput("one line".into()),
            row: Some("q back"),
            row_45: None,
            hints: &[("q", "back")],
            prep: |a| a.check_output_max_scroll.set(0),
        },
        ModalCase {
            name: "confirm",
            modal: confirm(None),
            row: Some("←→ pick   ↵ choose   q back"),
            row_45: None,
            hints: &[("←→", "pick"), ("↵", "choose"), ("q", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "confirm scroll",
            modal: confirm(Some("x".repeat(1200))),
            row: Some("←→ pick   ↵ choose   ↑↓ scroll   q back"),
            row_45: None,
            hints: &[
                ("←→", "pick"),
                ("↵", "choose"),
                ("↑↓", "scroll"),
                ("q", "back"),
            ],
            prep: |a| {
                if let Some(Modal::Confirm(s)) = a.modals.last_mut() {
                    s.scroll.max_scroll.set(1);
                    s.scroll.stamp.set(fresh_stamp());
                }
            },
        },
        ModalCase {
            name: "loss warning",
            modal: Modal::LossWarning(LossWarningState {
                focus: LossWarningButton::Save,
                target: LossWarningTarget::Switch(Tab::Usage),
                drop: LossWarningDrop {
                    dirty: vec![Dirty::SetupField],
                },
            }),
            row: Some("←→ pick   ↵ choose   esc · q go back"),
            row_45: None,
            hints: &[("←→", "pick"), ("↵", "choose"), ("esc · q", "go back")],
            prep: |_| {},
        },
        ModalCase {
            name: "preset apply",
            modal: preset_apply("apply 'p'?".into(), vec!["base url"]),
            row: Some("←→ pick   ↵ choose   q back"),
            row_45: None,
            hints: &[("←→", "pick"), ("↵", "choose"), ("q", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "preset apply scroll",
            modal: preset_apply("x".repeat(1200), vec!["base url"]),
            row: Some("←→ pick   ↵ choose   ↑↓ scroll   q back"),
            row_45: None,
            hints: &[
                ("←→", "pick"),
                ("↵", "choose"),
                ("↑↓", "scroll"),
                ("q", "back"),
            ],
            prep: |a| {
                if let Some(Modal::PresetApply(f)) = a.modals.last_mut() {
                    f.scroll.max_scroll.set(1);
                    f.scroll.stamp.set(fresh_stamp());
                }
            },
        },
        ModalCase {
            name: "divergence",
            modal: Modal::Divergence(DivergenceForm {
                active: "x".into(),
                sibling: None,
                // `discard` sits last so ↵ only opens its confirm, never the
                // overwrite path's credential work, inside a test.
                cursor: 2,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
            }),
            row: Some("↑↓ pick   ↵ choose   q back"),
            row_45: None,
            hints: &[("↑↓", "pick"), ("↵", "choose"), ("q", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "divergence target",
            modal: Modal::DivergenceTarget(DivergenceTargetForm {
                targets: vec!["b".into()],
                // A profile row, so ↵ takes the pick's toast path, never the
                // `+ new` capture flow.
                cursor: 1,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
            }),
            row: Some("↑↓ pick   ↵ choose   q back"),
            row_45: None,
            hints: &[("↑↓", "pick"), ("↵", "choose"), ("q", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "token picker",
            modal: Modal::TokenPicker(TokenPickerForm {
                targets: vec![("b".into(), false)],
                // A profile row, so ↵ opens the overwrite confirm, never the
                // `+ new profile` form.
                cursor: 1,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
                require: false,
            }),
            row: Some("↑↓ pick   ↵ choose   q back"),
            row_45: None,
            hints: &[("↑↓", "pick"), ("↵", "choose"), ("q", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "preset picker",
            modal: Modal::PresetPicker(PresetPickerForm {
                target: PresetTarget::NewAccount,
                presets: vec![preset.clone(), preset],
                cursor: 0,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
            }),
            row: Some("↑↓ pick   ↵ apply   d delete   q back"),
            row_45: None,
            hints: &[
                ("↑↓", "pick"),
                ("↵", "apply"),
                ("d", "delete"),
                ("q", "back"),
            ],
            prep: |_| {},
        },
        ModalCase {
            name: "preset picker builtin",
            modal: Modal::PresetPicker(PresetPickerForm {
                target: PresetTarget::NewAccount,
                presets: vec![builtin.clone(), builtin],
                cursor: 0,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
            }),
            row: Some("↑↓ pick   ↵ apply   q back"),
            row_45: None,
            hints: &[("↑↓", "pick"), ("↵", "apply"), ("q", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "action menu",
            modal: Modal::ActionMenu(ActionMenuState::new(
                vec![ActionMenuAction::TogglePace],
                vec![ActionMenuAction::ToggleEstimates],
                None,
                None,
            )),
            row: Some("↑↓ pick   ↵ choose   q back"),
            row_45: None,
            hints: &[("↑↓", "pick"), ("↵", "choose"), ("q", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "env collision",
            modal: collision(false),
            row: Some("↑↓ pick   ↵ choose   q back"),
            row_45: None,
            hints: &[("↑↓", "pick"), ("↵", "choose"), ("q", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "env collision review",
            modal: collision(true),
            row: Some("←→ pick   ↵ choose   q back"),
            row_45: None,
            hints: &[("←→", "pick"), ("↵", "choose"), ("q", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "env collision review scroll",
            modal: collision(true),
            row: None,
            row_45: None,
            hints: &[
                ("←→", "pick"),
                ("↵", "choose"),
                ("↑↓", "scroll"),
                ("q", "back"),
            ],
            prep: |a| {
                if let Some(Modal::EnvCollision(s)) = a.modals.last_mut() {
                    s.max_scroll.set(1);
                    s.max_scroll_stamp.set(fresh_stamp());
                }
            },
        },
        ModalCase {
            name: "capture name",
            modal: Modal::CaptureName(CaptureNameForm {
                snapshot: Box::new(CaptureSnapshot {
                    credentials: None,
                    base_url: None,
                    api_key: None,
                    anchor: AnchorAction::Unproven,
                    plan: None,
                }),
                input: InputState::new(""),
                from_divergence: false,
            }),
            row: Some("↵ save   ←→ caret   q back"),
            row_45: None,
            hints: &[("↵", "save"), ("←→", "caret"), ("q", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "name prompt",
            modal: Modal::NamePrompt(NamePromptForm {
                input: InputState::new(""),
                action: NamePromptAction::DuplicateProfile("src".into()),
            }),
            row: Some("↵ save   ←→ caret   esc back"),
            row_45: None,
            hints: &[("↵", "save"), ("←→", "caret"), ("esc", "back")],
            prep: |_| {},
        },
        ModalCase {
            name: "rebind",
            modal: Modal::Rebind(RebindForm {
                blocks: vec![RebindBlock {
                    rows: vec![RowMove {
                        row: "model",
                        old: None,
                        new: None,
                    }],
                    // Two accounts, so ↓ moves the cursor instead of wrapping
                    // onto itself.
                    accounts: vec!["a".into(), "b".into()],
                }],
                checked: vec![vec![false, false]],
                cursor: 0,
                choice: false,
                scroll: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
                viewport: Cell::new(0),
                digest: "d".into(),
            }),
            row: Some("↑↓ row   space toggle   ↵ choose   ? help   q not now"),
            row_45: Some("↑↓ row   space toggle   ? help   q not now"),
            hints: &[
                ("↑↓", "row"),
                ("space", "toggle"),
                ("↵", "choose"),
                ("q", "not now"),
            ],
            prep: |_| {},
        },
    ]
}

/// A stable index per `Modal` variant. Exhaustive (no wildcard): a new variant
/// fails to compile until it gets an arm here, and the coverage test below
/// fails until a case drives it. That guard fires only once `MODAL_COUNT` is
/// bumped past the new arm's index; an arm numbered at or past `MODAL_COUNT`
/// reds on first use via the assert in [`modal_selection`].
fn modal_index(modal: &Modal) -> usize {
    match modal {
        Modal::Help => 0,
        Modal::CheckOutput(_) => 1,
        Modal::Confirm(_) => 2,
        Modal::LossWarning(_) => 3,
        Modal::PresetApply(_) => 4,
        Modal::EnvCollision(_) => 5,
        Modal::Divergence(_) => 6,
        Modal::DivergenceTarget(_) => 7,
        Modal::TokenPicker(_) => 8,
        Modal::PresetPicker(_) => 9,
        Modal::ActionMenu(_) => 10,
        Modal::CaptureName(_) => 11,
        Modal::NamePrompt(_) => 12,
        Modal::Rebind(_) => 13,
        Modal::Login => 14,
        Modal::CodexLogin => 15,
    }
}

/// The number of `Modal` variants; [`modal_index`] returns `0..MODAL_COUNT`.
/// Bumped with `modal_index` — the coverage test reds when an index has no
/// case. That guard fires only once `MODAL_COUNT` is bumped past the new arm's
/// index.
const MODAL_COUNT: usize = 16;

/// Every `Modal` variant has a case driving its row; the two login modals are
/// the variants a case cannot cover (their login-in-flight lines own the
/// footer), so their indexes are skipped.
#[test]
fn the_modal_case_list_covers_every_variant() {
    let mut covered = [false; MODAL_COUNT];
    for case in modal_cases() {
        covered[modal_index(&case.modal)] = true;
    }
    let login = modal_index(&Modal::Login);
    let codex_login = modal_index(&Modal::CodexLogin);
    for (index, is_covered) in covered.iter().enumerate() {
        assert!(
            *is_covered || index == login || index == codex_login,
            "no modal case covers variant index {index}"
        );
    }
}

/// A stable fingerprint of the modal's movable state, so a drive can assert the
/// key did something without naming every field per modal. Exhaustive by
/// construction: a new `Modal` variant fails to compile here too.
fn modal_selection(app: &App) -> String {
    let top = app.modals.last();
    if let Some(modal) = top {
        let index = modal_index(modal);
        assert!(
            index < MODAL_COUNT,
            "modal arm {index} is at or past MODAL_COUNT {MODAL_COUNT}; bump MODAL_COUNT"
        );
    }
    match top {
        Some(Modal::Confirm(s)) => format!("{}:{}", s.choice, s.scroll.offset),
        Some(Modal::LossWarning(s)) => format!("{:?}", s.focus),
        Some(Modal::PresetApply(f)) => format!("{:?}:{}", f.focus, f.scroll.offset),
        Some(Modal::Divergence(f)) => format!("{}", f.cursor),
        Some(Modal::DivergenceTarget(f)) => format!("{}", f.cursor),
        Some(Modal::TokenPicker(f)) => format!("{}", f.cursor),
        Some(Modal::PresetPicker(f)) => format!("{}", f.cursor),
        Some(Modal::ActionMenu(f)) => format!("{}", f.cursor),
        Some(Modal::EnvCollision(f)) => format!(
            "{}:{}:{}:{}",
            f.cursor,
            f.reviewing_overwrite,
            f.confirm_overwrite,
            f.scroll.get()
        ),
        Some(Modal::CaptureName(f)) => format!("{}:{}", f.input.value, f.input.cursor),
        Some(Modal::NamePrompt(f)) => format!("{}:{}", f.input.value, f.input.cursor),
        Some(Modal::Rebind(f)) => format!("{}:{}:{:?}", f.choice, f.cursor, f.checked),
        Some(Modal::Help) => format!("{}", app.help_scroll),
        Some(Modal::CheckOutput(_)) => format!("{}", app.check_output_scroll),
        Some(Modal::Login) => String::new(),
        Some(Modal::CodexLogin) => String::new(),
        None => String::new(),
    }
}

/// A stable fingerprint of the keyboard owner's own movable state. Exhaustive
/// by construction over `KeyOwner`.
fn owner_selection(app: &App) -> String {
    let owner = keyboard_owner(app);
    if let Some(owner) = owner {
        let index = owner_index(owner);
        assert!(
            index < OWNER_COUNT,
            "owner arm {index} is at or past OWNER_COUNT {OWNER_COUNT}; bump OWNER_COUNT"
        );
    }
    match owner {
        Some(KeyOwner::Modal) => modal_selection(app),
        Some(KeyOwner::SetupField | KeyOwner::NewAccountField) => app
            .config_draft
            .as_ref()
            .map(|d| format!("{d:?}"))
            .unwrap_or_default(),
        Some(KeyOwner::MemberField) => app
            .fallback_edit
            .as_ref()
            .map(|e| format!("{e:?}"))
            .unwrap_or_default(),
        Some(KeyOwner::DayPicker) => {
            let days = app
                .config()
                .find(&crate::profile::ProfileName::from("a".to_string()))
                .map(|p| p.preferred_days.len())
                .unwrap_or(0);
            format!("{:?}|{}", app.fallback_edit, days)
        }
        Some(KeyOwner::RefreshInterval) => app
            .refresh_interval_draft
            .as_ref()
            .map(|i| format!("{i:?}"))
            .unwrap_or_default(),
        Some(KeyOwner::ContextNudge) => app
            .context_nudge_draft
            .as_ref()
            .map(|i| format!("{i:?}"))
            .unwrap_or_default(),
        Some(KeyOwner::WeeklyThreshold) => app
            .weekly_threshold_draft
            .as_ref()
            .map(|i| format!("{i:?}"))
            .unwrap_or_default(),
        Some(KeyOwner::HerdrTag) => app
            .services
            .herdr_tag_draft
            .as_ref()
            .map(|i| format!("{i:?}"))
            .unwrap_or_default(),
        Some(KeyOwner::ShuntField) => app
            .services
            .shunt_field_edit
            .as_ref()
            .map(|s| format!("{s:?}"))
            .unwrap_or_default(),
        Some(KeyOwner::NoteEditor) => app
            .note_editor
            .as_ref()
            .map(|n| format!("{n:?}"))
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// Every observable surface a drive can change: the top modal's variant and
/// state, the keyboard owner's variant and state, and the toast / quit / alert
/// surfaces.
fn fingerprint(app: &App) -> String {
    format!(
        "{:?}|{}|{:?}|{}|{}|{}|{}|{}",
        app.modals.last().map(std::mem::discriminant),
        modal_selection(app),
        keyboard_owner(app),
        owner_selection(app),
        app.toasts.len(),
        app.armed_quit.is_some(),
        app.footer_alert.is_some(),
        app.services.shunt_save.is_some(),
    )
}

/// Drive `event` through the real handler and report whether anything
/// observable changed. A control pins its own correctness (see
/// [`the_acted_probe_has_a_control`]).
fn acted(app: &mut App, event: ratatui::crossterm::event::KeyEvent) -> bool {
    let before = fingerprint(app);
    crate::tui::app::handle_key(app, event);
    before != fingerprint(app)
}

/// The footer shows the modal's own keys alone: every modal's full row, pinned
/// by equality at the two sizes the task names, with no screen-level group.
#[test]
fn the_each_modal_footer_shows_only_its_own_keys() {
    let _home = crate::testutil::HomeSandbox::new();
    for case in modal_cases() {
        let Some(row) = case.row else { continue };
        let mut app = bare_app();
        app.modals.push(case.modal.clone());
        for (w, h) in [(80u16, 24u16), (45u16, 24u16)] {
            let expected = if w == 45 {
                case.row_45.unwrap_or(row)
            } else {
                row
            };
            let footer = settled_footer(&app, w, h);
            assert_eq!(footer, expected, "{} footer at {w}x{h}", case.name);
        }
    }
}

/// The ruled rows where `←`/`→` act without a `←→` hint group: the re-bind
/// confirm's `←`/`→` flip the focused button, so its ruled copy leaves them
/// unhinted and the derivation drive names it instead of asserting inert.
const OMITTED_ARROWS: &[(&str, &str)] = &[("rebind", "←/→ flip the focused button")];

/// Every hinted key is derived: it acts through the real handler, the `↑↓`
/// lead is offered exactly where a press scrolls, and the screen-level keys the
/// row drops are genuinely inert under the modal (no tab switch, no action
/// menu, no quit arm).
#[test]
fn the_every_modal_hint_derives_from_the_handler() {
    use ratatui::crossterm::event::KeyCode;

    let _home = crate::testutil::HomeSandbox::new();

    for case in modal_cases() {
        let expected = case.hints;

        let mut app = bare_app();
        app.modals.push(case.modal.clone());
        (case.prep)(&mut app);
        assert_eq!(
            owner_hints(KeyOwner::Modal, &app, stamp_area()),
            Some(expected),
            "{}: the row derives from owner_hints",
            case.name
        );

        // Every hinted key acts through handle_key.
        for (hint, _) in expected {
            let mut app = bare_app();
            app.modals.push(case.modal.clone());
            (case.prep)(&mut app);
            let code = match *hint {
                "↑↓" => KeyCode::Down,
                "←→" if case.name == "capture name" || case.name == "name prompt" => {
                    KeyCode::Left
                }
                // The loss warning defaults to `save` (rightmost), so `→` is a
                // no-op there; `←` steps to `discard`.
                "←→" if case.name == "loss warning" => KeyCode::Left,
                "←→" => KeyCode::Right,
                "↵" => KeyCode::Enter,
                "space" => KeyCode::Char(' '),
                "esc" => KeyCode::Esc,
                "esc · q" => KeyCode::Esc,
                "q" => KeyCode::Char('q'),
                "d" => KeyCode::Char('d'),
                other => unreachable!("unmapped hint key {other}"),
            };
            // A typed field's caret sits at the end of an empty value, where →
            // is a no-op: give it a character so the arrow moves the caret.
            if *hint == "←→" && (case.name == "capture name" || case.name == "name prompt") {
                crate::tui::app::handle_key(&mut app, crate::testutil::key(KeyCode::Char('a')));
            }
            assert!(
                acted(&mut app, crate::testutil::key(code)),
                "{}: `{hint}` acts",
                case.name
            );
        }

        // The `↑↓` lead is offered exactly where a press scrolls: drive ↓ on
        // every modal and require it acts iff the row names it.
        let mut app = bare_app();
        app.modals.push(case.modal.clone());
        (case.prep)(&mut app);
        let has_updown = expected.iter().any(|(k, _)| *k == "↑↓");
        assert_eq!(
            acted(&mut app, crate::testutil::key(KeyCode::Down)),
            has_updown,
            "{}: ↓ acts iff the row offers ↑↓",
            case.name
        );

        // `←` acts iff the row offers `←→` — except the ruled rows in
        // [`OMITTED_ARROWS`], whose `←`/`→` act while the copy leaves them
        // unhinted. A typed-entry field's caret sits at the end of an empty
        // value, so give it a character first.
        let mut app = bare_app();
        app.modals.push(case.modal.clone());
        (case.prep)(&mut app);
        if case.name == "capture name" || case.name == "name prompt" {
            crate::tui::app::handle_key(&mut app, crate::testutil::key(KeyCode::Char('a')));
        }
        let has_lr = expected.iter().any(|(k, _)| *k == "←→");
        let left_acts = acted(&mut app, crate::testutil::key(KeyCode::Left));
        if let Some((_, reason)) = OMITTED_ARROWS.iter().find(|(n, _)| *n == case.name) {
            assert!(left_acts, "{}: {reason}", case.name);
        } else {
            assert_eq!(
                left_acts, has_lr,
                "{}: ← acts iff the row offers ←→",
                case.name
            );
        }

        // The screen-level keys the row drops are inert under the modal.
        let mut app = bare_app();
        app.modals.push(case.modal.clone());
        let tab_before = app.tab;
        crate::tui::app::handle_key(&mut app, crate::testutil::key(KeyCode::Left));
        crate::tui::app::handle_key(&mut app, crate::testutil::key(KeyCode::Right));
        assert_eq!(app.tab, tab_before, "{}: ←/→ do not switch tabs", case.name);
        let stack_before = app.modals.len();
        crate::tui::app::handle_key(&mut app, crate::testutil::key(KeyCode::Char('a')));
        assert_eq!(
            app.modals.len(),
            stack_before,
            "{}: a opens no action menu",
            case.name
        );

        let mut app = bare_app();
        app.modals.push(case.modal);
        crate::tui::app::handle_key(&mut app, crate::testutil::key(KeyCode::Char('q')));
        assert!(
            app.armed_quit.is_none(),
            "{}: q does not arm quit",
            case.name
        );
        assert!(
            app.footer_alert.is_none(),
            "{}: q raises no quit alert",
            case.name
        );
    }
}

/// At least one fixture per `KeyOwner` variant (the shunt field editor gets one
/// per row state): enter that owner state and derive its row, drive every
/// hinted key, and prove `←→` is inert exactly where the row sheds it (a `Pick`
/// cycle row has no caret).
#[test]
fn the_every_key_owner_hint_derives_from_the_handler() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let _home = crate::testutil::HomeSandbox::new();

    for case in owner_cases() {
        let OwnerCase {
            name,
            owner,
            hints,
            build,
        } = case;
        let app = build();
        assert_eq!(
            owner_hints(owner, &app, stamp_area()),
            Some(hints),
            "{name}: the row derives from owner_hints"
        );

        for (hint, _) in hints {
            let mut app = build();
            if *hint == "←→" {
                crate::tui::app::handle_key(&mut app, crate::testutil::key(KeyCode::Char('a')));
            }
            let event = match *hint {
                "↑↓" => crate::testutil::key(KeyCode::Down),
                "←→" => crate::testutil::key(KeyCode::Left),
                "↵" => crate::testutil::key(KeyCode::Enter),
                "space" => crate::testutil::key(KeyCode::Char(' ')),
                "esc" => crate::testutil::key(KeyCode::Esc),
                "q" => crate::testutil::key(KeyCode::Char('q')),
                "⌃j" => KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
                other => unreachable!("unmapped hint key {other}"),
            };
            assert!(acted(&mut app, event), "{name}: `{hint}` acts");
        }

        // `←→` is inert exactly where the row sheds it: a Pick cycle row has
        // no caret, a typed field's does.
        let mut app = build();
        crate::tui::app::handle_key(&mut app, crate::testutil::key(KeyCode::Char('a')));
        let has_lr = hints.iter().any(|(k, _)| *k == "←→");
        assert_eq!(
            acted(&mut app, crate::testutil::key(KeyCode::Left)),
            has_lr,
            "{name}: ←→ acts iff the row offers it"
        );
    }
}

/// One `KeyOwner` fixture: enter that owner state (`build`) and derive its row.
struct OwnerCase {
    name: &'static str,
    owner: KeyOwner,
    hints: HintRow,
    build: fn() -> App,
}

fn owner_cases() -> Vec<OwnerCase> {
    vec![
        OwnerCase {
            name: "setup field",
            owner: KeyOwner::SetupField,
            hints: &[("↵", "save"), ("←→", "caret"), ("esc", "revert")],
            build: setup_field_app,
        },
        OwnerCase {
            name: "new account field",
            owner: KeyOwner::NewAccountField,
            hints: &[("↵", "done"), ("←→", "caret"), ("esc", "done")],
            build: new_account_field_app,
        },
        OwnerCase {
            name: "member field",
            owner: KeyOwner::MemberField,
            hints: &[("↵", "save"), ("←→", "caret"), ("esc", "revert")],
            build: member_field_app,
        },
        OwnerCase {
            name: "day picker",
            owner: KeyOwner::DayPicker,
            hints: &[
                ("←→", "day"),
                ("space", "toggle"),
                ("↑↓", "row"),
                ("q", "back"),
            ],
            build: day_picker_app,
        },
        OwnerCase {
            name: "refresh interval",
            owner: KeyOwner::RefreshInterval,
            hints: &[("↵", "save"), ("←→", "caret"), ("esc", "revert")],
            build: refresh_interval_app,
        },
        OwnerCase {
            name: "context nudge",
            owner: KeyOwner::ContextNudge,
            hints: &[("↵", "save"), ("←→", "caret"), ("esc", "revert")],
            build: context_nudge_app,
        },
        OwnerCase {
            name: "weekly threshold",
            owner: KeyOwner::WeeklyThreshold,
            hints: &[("↵", "save"), ("←→", "caret"), ("esc", "revert")],
            build: weekly_threshold_app,
        },
        OwnerCase {
            name: "herdr tag",
            owner: KeyOwner::HerdrTag,
            hints: &[("↵", "save"), ("←→", "caret"), ("esc", "revert")],
            build: herdr_tag_app,
        },
        OwnerCase {
            name: "shunt draft pick",
            owner: KeyOwner::ShuntField,
            hints: &[("↵", "done"), ("esc", "revert")],
            build: shunt_draft_pick_app,
        },
        OwnerCase {
            name: "shunt pick",
            owner: KeyOwner::ShuntField,
            hints: &[("↵", "save"), ("esc", "revert")],
            build: shunt_pick_app,
        },
        OwnerCase {
            name: "shunt draft field",
            owner: KeyOwner::ShuntField,
            hints: &[("↵", "done"), ("←→", "caret"), ("esc", "revert")],
            build: shunt_draft_field_app,
        },
        OwnerCase {
            name: "shunt field locked",
            owner: KeyOwner::ShuntField,
            hints: &[("↑↓", "row"), ("q", "back")],
            build: shunt_locked_app,
        },
        OwnerCase {
            name: "note editor",
            owner: KeyOwner::NoteEditor,
            hints: &[
                ("↵", "save"),
                ("⌃j", "newline"),
                ("←→", "caret"),
                ("esc", "cancel"),
            ],
            build: note_editor_app,
        },
    ]
}

/// A stable index per `KeyOwner` variant. Exhaustive (no wildcard): a new
/// variant fails to compile until it gets an arm here, and the coverage test
/// below fails until a case drives it. That guard fires only once `OWNER_COUNT`
/// is bumped past the new arm's index; an arm numbered at or past `OWNER_COUNT`
/// reds on first use via the assert in [`owner_selection`].
fn owner_index(owner: KeyOwner) -> usize {
    match owner {
        KeyOwner::Modal => 0,
        KeyOwner::SetupField => 1,
        KeyOwner::NewAccountField => 2,
        KeyOwner::MemberField => 3,
        KeyOwner::DayPicker => 4,
        KeyOwner::RefreshInterval => 5,
        KeyOwner::ContextNudge => 6,
        KeyOwner::WeeklyThreshold => 7,
        KeyOwner::HerdrTag => 8,
        KeyOwner::ShuntField => 9,
        KeyOwner::NoteEditor => 10,
    }
}

/// The number of `KeyOwner` variants; [`owner_index`] returns `0..OWNER_COUNT`.
/// Bumped with `owner_index` — the coverage test reds when an index has no
/// case. That guard fires only once `OWNER_COUNT` is bumped past the new arm's
/// index.
const OWNER_COUNT: usize = 11;

/// Every `KeyOwner` variant has a case driving its row. The modal owner's row
/// is the top modal's, driven by [`modal_cases`], so its index is skipped.
#[test]
fn the_owner_case_list_covers_every_variant() {
    let mut covered = [false; OWNER_COUNT];
    for case in owner_cases() {
        covered[owner_index(case.owner)] = true;
    }
    let modal = owner_index(KeyOwner::Modal);
    for (index, is_covered) in covered.iter().enumerate() {
        assert!(
            *is_covered || index == modal,
            "no owner case covers variant index {index}"
        );
    }
}

fn setup_field_app() -> App {
    use crate::tui::app::{ConfigFocus, ConfigRow, InputState, build_draft_new};
    let mut app = bare_app();
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    let mut draft = build_draft_new();
    draft.editing_name = Some("a".into());
    draft.active = Some(ConfigRow::BaseUrl);
    draft.base_url = InputState::new("http://127.0.0.1:1234");
    app.config_draft = Some(draft);
    app
}

fn new_account_field_app() -> App {
    use crate::tui::app::{ConfigFocus, ConfigRow, InputState, build_draft_new};
    let mut app = bare_app();
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    let mut draft = build_draft_new();
    draft.active = Some(ConfigRow::BaseUrl);
    draft.base_url = InputState::new("http://127.0.0.1:1234");
    app.config_draft = Some(draft);
    app
}

fn member_field_app() -> App {
    use crate::tui::app::{CardEdit, FallbackFocus, InputState, MemberEdit};
    let mut app = bare_app();
    app.tab = Tab::Fallback;
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_edit = Some(MemberEdit {
        member: "a".into(),
        state: CardEdit::Threshold(InputState::new("70")),
        seed: Some("70".to_string()),
    });
    app
}

fn day_picker_app() -> App {
    use crate::profile::Profile;
    use crate::tui::app::{CardEdit, DayPicker, FallbackFocus, MemberEdit};
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec!["a".into()],
            fallback_chain: vec!["a".into()],
            ..AppState::default()
        },
        profiles: vec![Profile::new("a".to_string(), None, None)],
    });
    app.tab = Tab::Fallback;
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_edit = Some(MemberEdit {
        member: "a".into(),
        state: CardEdit::Days(DayPicker::default()),
        seed: None,
    });
    app
}

fn refresh_interval_app() -> App {
    use crate::tui::app::SeededInput;
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.refresh_interval_draft = Some(SeededInput::new("30"));
    app
}

fn context_nudge_app() -> App {
    use crate::tui::app::SeededInput;
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.context_nudge_draft = Some(SeededInput::new("300k"));
    app
}

fn weekly_threshold_app() -> App {
    use crate::tui::app::SeededInput;
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.weekly_threshold_draft = Some(SeededInput::new("50"));
    app
}

fn herdr_tag_app() -> App {
    use crate::tui::app::SeededInput;
    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.herdr_tag_draft = Some(SeededInput::new("30"));
    app
}

fn shunt_draft_pick_app() -> App {
    use crate::tui::app::{
        ShuntAddKind, ShuntDraftField, ShuntFieldEdit, ShuntFieldEditState, ShuntFieldKey,
        UpstreamFieldKind,
    };
    let key = ShuntFieldKey::Draft {
        kind: ShuntAddKind::Upstream,
        field: ShuntDraftField::Upstream(UpstreamFieldKind::Name),
    };
    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key,
        edit: ShuntFieldEdit::Pick {
            saved: None,
            picked: "api_key".into(),
        },
        refusal: None,
    });
    app
}

/// A non-draft field's pending pick: `↵ save` commits through the gateway
/// record (the save worker marks the row saving), `esc revert` closes it.
fn shunt_pick_app() -> App {
    use crate::tui::app::{
        ShuntConfigRead, ShuntFieldEdit, ShuntFieldEditState, ShuntFieldKey, UpstreamFieldKind,
    };
    let home = crate::profile::home_dir().expect("sandboxed home");
    let config = home.join("etc").join("shunt.toml");
    std::fs::create_dir_all(config.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &config,
        "[server]\nbind = \"127.0.0.1:9\"\n\n[[upstreams]]\nname = \"a\"\nauth = \"claude_oauth\"\n",
    )
    .expect("write config");
    let record = crate::gateway::GatewayRecord::new(config).expect("adoptable");
    let view = crate::gateway_edit::read_view(&record).expect("readable");
    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_record = Some(record);
    app.services.shunt_config = Some(ShuntConfigRead::Ok(Box::new(view)));
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key: ShuntFieldKey::Upstream {
            name: "a".into(),
            occurrence: 0,
            field: UpstreamFieldKind::Effort,
        },
        edit: ShuntFieldEdit::Pick {
            saved: None,
            picked: "low".into(),
        },
        refusal: None,
    });
    app
}

/// A draft field's typed editor: `↵ done` lands in the draft (nothing saves
/// until the form's own save row), `←→` move the caret, `esc` reverts.
fn shunt_draft_field_app() -> App {
    use crate::tui::app::{
        InputState, ShuntAddKind, ShuntDraftField, ShuntFieldEdit, ShuntFieldEditState,
        ShuntFieldKey, UpstreamFieldKind,
    };
    let key = ShuntFieldKey::Draft {
        kind: ShuntAddKind::Upstream,
        field: ShuntDraftField::Upstream(UpstreamFieldKind::Name),
    };
    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key,
        edit: ShuntFieldEdit::Typed {
            input: InputState::new("x"),
            saved: None,
        },
        refusal: None,
    });
    app
}

fn shunt_locked_app() -> App {
    use crate::tui::app::{
        InputState, ShuntFieldEdit, ShuntFieldEditState, ShuntFieldKey, ShuntSave, ShuntSaveKey,
        UpstreamFieldKind,
    };
    use std::time::Instant;
    let key = ShuntFieldKey::Upstream {
        name: "a".into(),
        occurrence: 0,
        field: UpstreamFieldKind::Effort,
    };
    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key: key.clone(),
        edit: ShuntFieldEdit::Typed {
            input: InputState::new("x"),
            saved: None,
        },
        refusal: None,
    });
    app.services.shunt_save = Some(ShuntSave {
        key: ShuntSaveKey::Field(key),
        started: Instant::now(),
        value: None,
        entry: None,
    });
    app
}

fn note_editor_app() -> App {
    use crate::tui::app::{NoteBuffer, NoteEditor};
    let mut app = bare_app();
    app.tab = Tab::Usage;
    app.note_editor = Some(NoteEditor {
        profile: "a".into(),
        seed: "x".to_string(),
        buf: NoteBuffer::new("x"),
    });
    app
}

/// The `acted` probe's own control: an inert key reads `!acted`, and a state
/// change reads `acted`, so a broken `acted` (always true or always false) reds.
#[test]
fn the_acted_probe_has_a_control() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.modals.push(Modal::Confirm(ConfirmState {
        message: "rotate 'x'?".into(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RotateOne("x".into()),
    }));
    assert!(
        !acted(&mut app, crate::testutil::key(KeyCode::Char('?'))),
        "? is inert on a confirm"
    );
    assert!(
        acted(&mut app, crate::testutil::key(KeyCode::Right)),
        "→ flips the confirm choice"
    );
}

/// A login in flight keeps its lead line and shows the open modal's own row
/// behind it (not the base `q back`, and never `q quit`). The name prompt reads
/// `esc back` there, so the bare `cancel` can't read as the login's.
#[test]
fn the_login_footer_shows_the_open_modals_row() {
    use crate::tui::app::{
        ActionMenuAction, ActionMenuState, ConfirmAction, ConfirmScroll, ConfirmState, InputState,
        LoginMethod, LoginStage, NamePromptAction, NamePromptForm,
    };
    let _home = crate::testutil::HomeSandbox::new();

    let login = || LoginSession {
        name: "fresh".into(),
        is_new: false,
        generation: 0,
        url: None,
        stage: LoginStage::WaitingBrowser,
        method: LoginMethod::Browser,
        paste: None,
        paste_field: None,
    };
    let confirm = Modal::Confirm(ConfirmState {
        message: "rotate 'x'?".into(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RotateOne("x".into()),
    });
    let action_menu = Modal::ActionMenu(ActionMenuState::new(
        vec![ActionMenuAction::TogglePace],
        vec![],
        None,
        None,
    ));
    let name_prompt = Modal::NamePrompt(NamePromptForm {
        input: InputState::new(""),
        action: NamePromptAction::DuplicateProfile("src".into()),
    });

    let cases: Vec<(&str, Modal, &str, &str)> = vec![
        (
            "help",
            Modal::Help,
            "⠋ logging in 'fresh'   ↑↓ scroll   q back",
            "⠋ logging in 'fresh'   ↑↓ scroll   q back",
        ),
        (
            "confirm",
            confirm,
            "⠋ logging in 'fresh'   ←→ pick   ↵ choose   q back",
            "⠋ logging in 'fresh'   ←→ pick   q back",
        ),
        (
            "action menu",
            action_menu,
            "⠋ logging in 'fresh'   ↑↓ pick   ↵ choose   q back",
            "⠋ logging in 'fresh'   ↑↓ pick   q back",
        ),
        (
            "name prompt",
            name_prompt,
            "⠋ logging in 'fresh'   ↵ save   ←→ caret   esc back",
            "⠋ logging in 'fresh'   ↵ save   esc back",
        ),
    ];

    for (name, modal, row_80, row_45) in cases {
        for (w, h, expected) in [(80u16, 24u16, row_80), (45u16, 24u16, row_45)] {
            let mut app = bare_app();
            app.login = Some(login());
            app.modals.push(modal.clone());
            let footer = settled_footer(&app, w, h);
            assert_eq!(footer, expected, "{name} login footer at {w}x{h}");
        }
    }
}

/// A codex re-login in flight keeps its lead line and reads `q back` while its
/// modal is open (the collapse key; `r`/`c` live in the modal body) and `esc
/// cancel` once collapsed — the [`draw_login`] shape without the paste door.
#[test]
fn the_codex_login_footer_names_only_its_modal_keys() {
    use crate::tui::app::CodexLoginSession;
    let _home = crate::testutil::HomeSandbox::new();

    let session = || CodexLoginSession {
        name: "cdx-one".into(),
        generation: 0,
        url: None,
        cancel: crate::codex_login::LoginCancel::new(),
        is_new: false,
    };

    let mut open = bare_app();
    open.codex_login = Some(session());
    open.modals.push(Modal::CodexLogin);
    assert_eq!(
        settled_footer(&open, 80, 24),
        "⠋ logging in 'cdx-one'   q back",
        "the open codex login modal's footer"
    );

    let mut collapsed = bare_app();
    collapsed.codex_login = Some(session());
    assert_eq!(
        settled_footer(&collapsed, 80, 24),
        "⠋ logging in 'cdx-one'   esc cancel",
        "the collapsed codex login's footer"
    );
}

/// A codex re-login in flight keeps the claude login line's open-modal rule:
/// any open non-login modal's own keys ride behind the codex lead, never
/// `q back` where `q` types (the name prompt). Mirrors
/// `the_login_footer_shows_the_open_modals_row` with `codex_login` set.
#[test]
fn the_codex_login_footer_shows_the_open_modals_row() {
    use crate::tui::app::{
        ActionMenuAction, ActionMenuState, CodexLoginSession, ConfirmAction, ConfirmScroll,
        ConfirmState, InputState, NamePromptAction, NamePromptForm,
    };
    let _home = crate::testutil::HomeSandbox::new();

    let session = || CodexLoginSession {
        name: "cdx-one".into(),
        generation: 0,
        url: None,
        cancel: crate::codex_login::LoginCancel::new(),
        is_new: false,
    };
    let confirm = Modal::Confirm(ConfirmState {
        message: "rotate 'x'?".into(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RotateOne("x".into()),
    });
    let action_menu = Modal::ActionMenu(ActionMenuState::new(
        vec![ActionMenuAction::TogglePace],
        vec![],
        None,
        None,
    ));
    let name_prompt = Modal::NamePrompt(NamePromptForm {
        input: InputState::new(""),
        action: NamePromptAction::DuplicateProfile("src".into()),
    });

    let cases: Vec<(&str, Modal, &str, &str)> = vec![
        (
            "help",
            Modal::Help,
            "⠋ logging in 'cdx-one'   ↑↓ scroll   q back",
            "⠋ logging in 'cdx-one'   ↑↓ scroll   q back",
        ),
        (
            "confirm",
            confirm,
            "⠋ logging in 'cdx-one'   ←→ pick   ↵ choose   q back",
            "⠋ logging in 'cdx-one'   ←→ pick   q back",
        ),
        (
            "action menu",
            action_menu,
            "⠋ logging in 'cdx-one'   ↑↓ pick   ↵ choose   q back",
            "⠋ logging in 'cdx-one'   ↑↓ pick   q back",
        ),
        (
            "name prompt",
            name_prompt,
            "⠋ logging in 'cdx-one'   ↵ save   ←→ caret   esc back",
            "⠋ logging in 'cdx-one'   ↵ save   esc back",
        ),
    ];

    for (name, modal, row_80, row_45) in cases {
        for (w, h, expected) in [(80u16, 24u16, row_80), (45u16, 24u16, row_45)] {
            let mut app = bare_app();
            app.codex_login = Some(session());
            app.modals.push(modal.clone());
            let footer = settled_footer(&app, w, h);
            assert_eq!(footer, expected, "{name} codex login footer at {w}x{h}");
        }
    }
}

/// With the base frame order the footer draws before the modal, so the bound it
/// reads is the previous frame's. The stamp rule keeps that honest: a bound
/// whose stamp does not match this frame's area (a resize) or the modal-open
/// generation (an open or close) errs to the no-scroll row.
#[test]
fn the_footer_reads_the_scroll_bound_on_the_first_frame() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};
    let _home = crate::testutil::HomeSandbox::new();

    // Open help at 80x24 (it overflows), then resize to 200x100 where it fits:
    // the one frame after the resize must already drop the dead `↑↓ scroll` —
    // the bound is stamped with the 80x24 area, which no longer matches.
    let mut app = bare_app();
    app.modals.push(Modal::Help);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    term.backend_mut().resize(200, 100);
    term.autoresize().unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let footer = rows.last().expect("the footer row").trim();
    assert_eq!(
        footer, "q back",
        "help at 200x100 shows no dead ↑↓ scroll on the first resized frame"
    );

    // A long confirm's first frame errs to the no-scroll row — the footer draws
    // before the modal, so the bound is not published yet — and the second
    // frame names its scroll.
    let mut app = bare_app();
    app.modals.push(Modal::Confirm(ConfirmState {
        message: "rotate 'x'?".into(),
        detail: Some("x".repeat(1200)),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RotateOne("x".into()),
    }));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let footer = rows.last().expect("the footer row").trim();
    assert_eq!(
        footer, "←→ pick   ↵ choose   q back",
        "a long confirm's first frame errs to the no-scroll row"
    );
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let footer = rows.last().expect("the footer row").trim();
    assert_eq!(
        footer, "←→ pick   ↵ choose   ↑↓ scroll   q back",
        "a long confirm's second frame names its ↑↓ scroll"
    );
}

/// A bound that survives a close is not trusted after a reopen: the close and
/// the reopen bump the modal-open generation and the resize changes the area,
/// so the first frame after the reopen errs to `q back` whatever the size.
#[test]
fn a_reopened_modal_never_shows_a_stale_scroll() {
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = bare_app();
    app.open_modal(Modal::Help);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    // `help_max_scroll` survives the close, then a resize and a reopen leave it
    // stale for the footer's first frame.
    app.modals.pop();
    term.backend_mut().resize(200, 100);
    term.autoresize().unwrap();
    app.open_modal(Modal::Help);
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let footer = rows.last().expect("the footer row").trim();
    assert_eq!(
        footer, "q back",
        "a help modal reopened after a resize shows no stale ↑↓ scroll on its first frame"
    );
}

/// The generation counter alone errs a stale bound: a reopen at the same size
/// leaves the area matching, but the open and close bumped the generation, so
/// the first frame still errs to `q back` and the second shows the bound.
#[test]
fn a_reopened_modal_errs_until_it_redraws() {
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = bare_app();
    app.open_modal(Modal::Help);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    app.modals.pop();
    app.open_modal(Modal::Help);
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let footer = rows.last().expect("the footer row").trim();
    assert_eq!(
        footer, "q back",
        "a help modal reopened at the same size errs to q back on its first frame"
    );
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let footer = rows.last().expect("the footer row").trim();
    assert_eq!(
        footer, "↑↓ scroll   q back",
        "the second frame after a same-size reopen shows the scroll"
    );
}

/// The env-collision list stage must not refresh the review stage's scroll
/// stamp: only a frame that drew the review stage stamps its bound, so a
/// resize while the list is open leaves the review bound stale and the first
/// frame after re-entering review errs to the no-scroll row instead of a dead
/// `↑↓ scroll`.
#[test]
fn the_env_collision_review_never_shows_a_stale_scroll_after_a_resize() {
    use crate::tui::app::EnvCollisionForm;
    use ratatui::crossterm::event::KeyCode;
    use std::cell::Cell;
    let _home = crate::testutil::HomeSandbox::new();

    let form = |reviewing_overwrite: bool| {
        Modal::EnvCollision(EnvCollisionForm {
            profile: "a".into(),
            key: "K".into(),
            reason: "set by the base url field ".repeat(80),
            existing_idx: None,
            cursor: 0,
            scroll: Cell::new(0),
            reviewing_overwrite,
            confirm_overwrite: false,
            max_scroll: Cell::new(0),
            max_scroll_stamp: Cell::new(ScrollStamp::default()),
            viewport: Cell::new(0),
            overwrite_needs_review: Cell::new(true),
        })
    };
    let footer = |term: &ratatui::Terminal<ratatui::backend::TestBackend>| {
        crate::testutil::buffer_rows(term.backend().buffer())
            .last()
            .expect("the footer row")
            .trim()
            .to_string()
    };

    // One probe per entry route into the review stage. The handler route
    // re-enters via ↵ on the overwrite option, which the list draw only offers
    // while the list overflows, so it resizes to 80x30 (list still overflows);
    // the direct field flip resizes to 200x100, where the list fits and the
    // flip is the only way back in.
    struct ReentryCase {
        name: &'static str,
        w: u16,
        h: u16,
        settled: &'static str,
        enter: fn(&mut App),
    }
    let cases = [
        ReentryCase {
            name: "the handler's overwrite activation",
            w: 80,
            h: 30,
            settled: "←→ pick   ↵ choose   ↑↓ scroll   q back",
            enter: |app| {
                crate::tui::app::handle_key(app, crate::testutil::key(KeyCode::Enter));
            },
        },
        ReentryCase {
            name: "a direct field flip",
            w: 200,
            h: 100,
            settled: "←→ pick   ↵ choose   q back",
            enter: |app| {
                if let Some(Modal::EnvCollision(s)) = app.modals.last_mut() {
                    s.reviewing_overwrite = true;
                }
            },
        },
    ];

    for ReentryCase {
        name,
        w,
        h,
        settled,
        enter,
    } in cases
    {
        // Review at 80x24: the long consequence overflows, so frame two names
        // the scroll — the bound is real and published there.
        let mut app = bare_app();
        app.modals.push(form(true));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
        assert_eq!(
            footer(&term),
            "←→ pick   ↵ choose   ↑↓ scroll   q back",
            "{name}: the review stage overflows at 80x24 on frame two"
        );

        // Back to the list stage, resize, and let the list frames render.
        if let Some(Modal::EnvCollision(s)) = app.modals.last_mut() {
            s.reviewing_overwrite = false;
        }
        term.backend_mut().resize(w, h);
        term.autoresize().unwrap();
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();

        // Re-enter review: the first frame must drop the stale `↑↓ scroll`,
        // and the second settles on the row for the resized body.
        enter(&mut app);
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
        assert_eq!(
            footer(&term),
            "←→ pick   ↵ choose   q back",
            "{name}: the first frame after re-entry shows no stale ↑↓ scroll"
        );
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
        assert_eq!(
            footer(&term),
            settled,
            "{name}: the second frame settles on the row for {w}x{h}"
        );
    }
}

/// Behind a login in flight a long profile name never crowds out the modal's
/// essential groups: the name truncates with a trailing `…` so `esc back` (the
/// name prompt's way out) survives at 45 columns.
#[test]
fn the_login_footer_truncates_a_long_profile_name() {
    use crate::tui::app::{InputState, LoginMethod, LoginStage, NamePromptAction, NamePromptForm};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.login = Some(LoginSession {
        name: "a-very-long-profile".into(),
        is_new: false,
        generation: 0,
        url: None,
        stage: LoginStage::WaitingBrowser,
        method: LoginMethod::Browser,
        paste: None,
        paste_field: None,
    });
    app.modals.push(Modal::NamePrompt(NamePromptForm {
        input: InputState::new(""),
        action: NamePromptAction::DuplicateProfile("src".into()),
    }));
    let footer = settled_footer(&app, 45, 24);
    assert_eq!(
        footer, "⠋ logging in 'a-very-long-prof…'   esc back",
        "the long name truncates so the name prompt's esc back survives"
    );
}

/// A live footer alert replaces the hint bar in the one footer row, inset one
/// cell then `! `, and the hints return once it clears. This is a
/// characterisation: the behaviour is pre-existing (green on the pre-change
/// code), so it has no red-first run; the returned rows are pinned by equality
/// (the leading inset included).
#[test]
fn the_footer_alert_replaces_the_hints_in_place() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Usage;
    let at_rest = settled_footer(&app, 80, 24);

    app.footer_alert = Some(FooterAlert::Warn("press q again to quit".into()));
    for (w, h) in [(80u16, 24u16), (45u16, 24u16)] {
        let rows = settled_rows(&app, w, h);
        // The untrimmed row keeps the 1-cell inset before `! `; only the
        // trailing fill trims.
        let footer = rows.last().expect("the footer row").trim_end();
        assert_eq!(
            footer, " ! press q again to quit",
            "the alert replaces the hints with its 1-cell inset at {w}x{h}, got {footer:?}"
        );
    }

    app.footer_alert = None;
    let footer = settled_footer(&app, 80, 24);
    assert_eq!(
        footer, at_rest,
        "the hints return when the alert clears, got {footer:?}"
    );
}

/// The running-write save cue: ` i saving {row}… · ⌃c ⌃c quit now`, the row
/// name cut (grapheme-safe, one `…`) so the leader and `· ⌃c ⌃c quit now`
/// stay whole down to the 34-column floor. Shown during the exit wait on every
/// tab and during a quit held on a checking `+ add` save, on both tiers; every
/// cell's style is pinned (leader INFO, copy TEXT_DIM, key run ACCENT + bold).
#[test]
fn the_save_cue_names_the_row_and_survives_the_floor() {
    use ratatui::style::Modifier;

    fn save(key: ShuntSaveKey) -> ShuntSave {
        ShuntSave {
            key,
            started: std::time::Instant::now(),
            value: None,
            entry: None,
        }
    }

    fn footer_at(app: &App, w: u16, h: u16) -> (String, Vec<ratatui::buffer::Cell>) {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("backend");
        term.draw(|f| crate::tui::render::draw(f, app))
            .expect("draw");
        let buf = term.backend().buffer().clone();
        let footer = buf.area.height as usize - 1;
        let base = footer * buf.area.width as usize;
        let row = crate::testutil::buffer_rows(&buf)[footer].clone();
        (
            row,
            buf.content[base..base + buf.area.width as usize].to_vec(),
        )
    }

    fn check(app: &App, name: &str, w: u16) {
        let (row, cells) = footer_at(app, w, 24);
        let expected = format!(" i saving {name}\u{2026} · \u{2303}c \u{2303}c quit now");
        assert_eq!(row.trim_end(), expected, "whole row at {w}, got {row:?}");
        let key_start = format!(" i saving {name}\u{2026} · ").chars().count();
        for (i, cell) in cells.iter().enumerate().take(expected.chars().count()) {
            let (fg, bold) = if i == 0 {
                (crate::tui::theme::text_color(), false)
            } else if i < 3 {
                (crate::tui::theme::info_color(), false)
            } else if (key_start..key_start + 5).contains(&i) {
                (crate::tui::theme::accent_color(), true)
            } else {
                (crate::tui::theme::text_dim_color(), false)
            };
            assert_eq!(cell.fg, fg, "cell {i} fg at {w}, got {:?}", cell.fg);
            assert_eq!(
                cell.modifier.contains(Modifier::BOLD),
                bold,
                "cell {i} bold at {w}"
            );
            assert_eq!(cell.bg, crate::tui::theme::bg(), "cell {i} bg at {w}");
        }
    }

    let _home = crate::testutil::HomeSandbox::new();

    for tier in [
        crate::tui::theme::Tier::Full,
        crate::tui::theme::Tier::Compatible,
    ] {
        let _tier = crate::testutil::TierSandbox::new(tier);
        for tab in [Tab::Services, Tab::Tokens] {
            let mut app = bare_app();
            app.tab = tab;
            app.services.shunt_save = Some(save(ShuntSaveKey::Field(ShuntFieldKey::Bind)));
            app.exit_wait = true;
            check(&app, "bind", 80);
            check(&app, "bind", 34);

            let mut app = bare_app();
            app.tab = tab;
            app.services.shunt_save = Some(save(ShuntSaveKey::SaveAdd(ShuntAddKind::Upstream)));
            app.exit_wait = true;
            check(&app, "upstream", 80);
            check(&app, "upstr", 34);

            let mut app = bare_app();
            app.tab = tab;
            app.services.shunt_save = Some(save(ShuntSaveKey::Remove(ShuntEntryKey::Upstream {
                name: "a".into(),
                occurrence: 0,
            })));
            app.exit_wait = true;
            check(&app, "a", 80);
            check(&app, "a", 34);

            let mut app = bare_app();
            app.tab = tab;
            app.pool_mutation_row = Some((
                PoolFamily::Claude,
                ShuntAccountName::new("alpha").expect("valid"),
            ));
            app.pool_mutation_kind = Some(PoolMutationKind::Remove);
            app.exit_wait = true;
            check(&app, "alpha", 80);
            check(&app, "alpha", 34);

            let mut app = bare_app();
            app.tab = tab;
            app.services.shunt_action.running = true;
            app.exit_wait = true;
            check(&app, "shunt", 80);
            check(&app, "shunt", 34);
        }
    }

    // A quit held on a checking `+ add` save shows the same cue, computed
    // from `loss_pending`, not `exit_wait`.
    for tier in [
        crate::tui::theme::Tier::Full,
        crate::tui::theme::Tier::Compatible,
    ] {
        let _tier = crate::testutil::TierSandbox::new(tier);
        let mut app = bare_app();
        app.tab = Tab::Overview;
        app.services.shunt_save = Some(save(ShuntSaveKey::SaveAdd(ShuntAddKind::Upstream)));
        app.loss_pending = Some(LossWarningTarget::Quit);
        check(&app, "upstream", 80);
    }

    // The hints return once the wait ends.
    let mut app = bare_app();
    app.tab = Tab::Services;
    let at_rest = settled_footer(&app, 80, 24);
    app.services.shunt_save = Some(save(ShuntSaveKey::Field(ShuntFieldKey::Bind)));
    app.exit_wait = true;
    check(&app, "bind", 80);
    app.exit_wait = false;
    app.services.shunt_save = None;
    assert_eq!(
        settled_footer(&app, 80, 24),
        at_rest,
        "the services hints return when the wait ends"
    );
}

/// The save cue's rendered footer row (trailing blanks trimmed only — the
/// leading inset stays), for pins that assert the whole-row spelling.
fn cue_row(app: &App, w: u16) -> String {
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, 24)).expect("backend");
    term.draw(|f| crate::tui::render::draw(f, app))
        .expect("draw");
    let buf = term.backend().buffer().clone();
    let footer = buf.area.height as usize - 1;
    crate::testutil::buffer_rows(&buf)[footer]
        .trim_end()
        .to_string()
}

/// A save in flight for [`cue_row`] pins, keyed by `key`.
fn cue_save(key: ShuntSaveKey) -> ShuntSave {
    ShuntSave {
        key,
        started: std::time::Instant::now(),
        value: None,
        entry: None,
    }
}

/// The whole save-cue row for `name` interpolated into the copy, at width `w`.
fn check_cue(app: &App, name: &str, w: u16) {
    let row = cue_row(app, w);
    let expected = format!(" i saving {name}\u{2026} · \u{2303}c \u{2303}c quit now");
    assert_eq!(row, expected, "whole row at {w}, got {row:?}");
}

/// An untrusted name (an upstream, model or token entry, a model
/// `upstream_model` provider key) renders the escaped spelling the card shows —
/// a control or bidi character spelled `\u{…}`, never a raw byte.
#[test]
fn the_save_cue_escapes_untrusted_names_like_the_card() {
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_save = Some(cue_save(ShuntSaveKey::Remove(ShuntEntryKey::Upstream {
        name: "x\u{202e}y".into(),
        occurrence: 0,
    })));
    app.exit_wait = true;
    check_cue(&app, "x\\u{202e}y", 80);

    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_save = Some(cue_save(ShuntSaveKey::Token("p\u{1b}q".into())));
    app.exit_wait = true;
    check_cue(&app, "p\\u{1b}q", 80);

    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_save = Some(cue_save(ShuntSaveKey::Remove(ShuntEntryKey::Model {
        id: "m\u{1b}n".into(),
        occurrence: 0,
    })));
    app.exit_wait = true;
    check_cue(&app, "m\\u{1b}n", 80);

    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_save = Some(cue_save(ShuntSaveKey::Field(ShuntFieldKey::Model {
        id: "m".into(),
        occurrence: 0,
        field: ModelFieldKind::UpstreamModel("p\u{202e}q\u{1b}z".into()),
    })));
    app.exit_wait = true;
    check_cue(&app, "p\\u{202e}q\\u{1b}z", 80);
}

/// A cut name ends in exactly the always-on `…` — a kept trailing `…` is
/// trimmed before it — while an uncut name keeps its own characters.
#[test]
fn the_save_cue_cuts_a_name_to_one_ellipsis() {
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_save = Some(cue_save(ShuntSaveKey::Remove(ShuntEntryKey::Upstream {
        name: "abcd\u{2026}wxyz\u{2026}".into(),
        occurrence: 0,
    })));
    app.exit_wait = true;
    check_cue(&app, "abcd", 34);

    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_save = Some(cue_save(ShuntSaveKey::Remove(ShuntEntryKey::Upstream {
        name: "abc\u{2026}".into(),
        occurrence: 0,
    })));
    app.exit_wait = true;
    check_cue(&app, "abc\u{2026}", 80);
    check_cue(&app, "abc\u{2026}", 34);
}

/// A route remove whose config view is unreadable names the kind word
/// `route`, never `route N` (no card row reads that way).
#[test]
fn the_save_cue_names_a_vanished_route_route() {
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_save = Some(cue_save(ShuntSaveKey::Remove(ShuntEntryKey::Route(3))));
    app.exit_wait = true;
    check_cue(&app, "route", 80);
}

/// When a save, a `shunt_action` job and a pool remove run at once, the cue
/// names them in the ruled order — the save, then the action, then the pool
/// account — clearing each in turn as its write settles.
#[test]
fn the_save_cue_names_the_save_before_the_action_before_the_pool() {
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = bare_app();
    app.tab = Tab::Services;
    app.services.shunt_save = Some(cue_save(ShuntSaveKey::Field(ShuntFieldKey::Bind)));
    app.services.shunt_action.running = true;
    app.pool_mutation_row = Some((
        PoolFamily::Claude,
        ShuntAccountName::new("alpha").expect("valid"),
    ));
    app.pool_mutation_kind = Some(PoolMutationKind::Remove);
    app.exit_wait = true;
    check_cue(&app, "bind", 80);

    app.services.shunt_save = None;
    check_cue(&app, "shunt", 80);

    app.services.shunt_action.running = false;
    check_cue(&app, "alpha", 80);
}

/// A Setup `+ new` form of `account_type` with its cursor on `+ login` and the
/// cached shunt reason seeded — the state the footer's `+ login` hint reads.
/// The caller holds the `HomeSandbox` across `App::new`.
fn plus_new_login_app(account_type: crate::tui::app::NewAccountType, reason: Option<&str>) -> App {
    let mut app = bare_app();
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    app.profile_cursor.set(app.profile_count());
    let mut draft = crate::tui::app::build_draft_new();
    draft.account_type = account_type;
    app.config_draft = Some(draft);
    app.shunt_add_disabled = reason.map(str::to_string);
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("the form lists a `+ login` row");
    app
}

/// An existing claude OAuth account (a `re-login` row) with its cursor on the
/// login row and the cached shunt reason seeded — the state the footer's login
/// hint reads for a saved account.
fn relogin_app(reason: Option<&str>) -> App {
    let mut profile = crate::profile::Profile::new("acct-one".to_string(), None, None);
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    });
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    app.profile_cursor.set(0);
    app.shunt_add_disabled = reason.map(str::to_string);
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("the account lists a login row");
    app
}

/// R4-2 (claude footer): a cached shunt reason never strips the claude `+ new`
/// form's `↵` hint from its `+ login` row.
#[test]
fn a_claude_new_form_login_row_keeps_enter_despite_a_cached_shunt_reason() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = plus_new_login_app(
        crate::tui::app::NewAccountType::Claude,
        Some("no shunt gateway is adopted"),
    );
    assert_eq!(
        tab_hints(&app),
        [
            ("↑↓", "row"),
            ("↵", "edit / toggle"),
            ("a", "actions"),
            ("?", "help"),
            ("q", "back"),
        ],
        "the claude `+ login` keeps its ↵ hint"
    );
}

/// R4-2 (codex footer): the codex `+ new` form's `+ login` keeps its `↵` too.
#[test]
fn a_codex_new_form_login_row_keeps_enter_despite_a_cached_shunt_reason() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = plus_new_login_app(
        crate::tui::app::NewAccountType::Codex,
        Some("no shunt gateway is adopted"),
    );
    assert_eq!(
        tab_hints(&app),
        [
            ("↑↓", "row"),
            ("↵", "edit / toggle"),
            ("a", "actions"),
            ("?", "help"),
            ("q", "back"),
        ],
        "the codex `+ login` keeps its ↵ hint"
    );
}

/// R4-2 (re-login footer): an existing claude account's `re-login` row keeps
/// its `↵` hint even with a cached shunt reason — the reason is the shunt
/// form's alone.
#[test]
fn an_existing_accounts_login_row_keeps_enter_despite_a_cached_shunt_reason() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = relogin_app(Some("no shunt gateway is adopted"));
    assert_eq!(
        tab_hints(&app),
        [
            ("↑↓", "row"),
            ("↵", "edit / toggle"),
            ("a", "actions"),
            ("?", "help"),
            ("q", "back"),
        ],
        "the existing account's login row keeps its ↵ hint"
    );
}

/// R4-2 (shunt footer): the shunt `+ new` form's disabled `+ login` still
/// drops its `↵` hint — the disabled state is the shunt form's alone.
#[test]
fn a_shunt_new_form_login_row_drops_enter_while_disabled() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = plus_new_login_app(
        crate::tui::app::NewAccountType::Shunt,
        Some("no shunt gateway is adopted"),
    );
    assert_eq!(
        tab_hints(&app),
        [
            ("↑↓", "row"),
            ("a", "actions"),
            ("?", "help"),
            ("q", "back"),
        ],
        "the disabled shunt `+ login` drops its ↵ hint"
    );
}

/// No modal body row opens in sentence case (a capital then a lowercase
/// letter, `Discard …`): every case's interior rows are scanned at 80x24.
/// Residual: only the bodies these fixtures produce are seen; a body built
/// from other copy is covered by its own pin.
#[test]
fn no_modal_body_row_opens_in_sentence_case() {
    let _home = crate::testutil::HomeSandbox::new();
    for case in modal_cases() {
        let mut app = bare_app();
        app.modals.push(case.modal.clone());
        (case.prep)(&mut app);
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("backend");
        term.draw(|f| crate::tui::render::draw(f, &app))
            .expect("draw");
        term.draw(|f| crate::tui::render::draw(f, &app))
            .expect("draw");
        let buf = term.backend().buffer();
        // The modal is the box whose top-left corner is ACCENT_2; the panels
        // behind it draw the same glyph in their border colour.
        let orange = crate::tui::theme::accent_2_color();
        let corner = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .find(|&(x, y)| buf[(x, y)].symbol() == "\u{256d}" && buf[(x, y)].fg == orange);
        let (left, top) = corner.unwrap_or_else(|| panic!("{}: no open modal", case.name));
        let right = (left + 1..buf.area.width)
            .find(|&x| buf[(x, top)].symbol() == "\u{256e}")
            .expect("the modal's top-right corner");
        let bottom = (top + 1..buf.area.height)
            .find(|&y| buf[(left, y)].symbol() == "\u{2570}")
            .expect("the modal's bottom-left corner");
        for y in top + 1..bottom {
            let interior: String = (left + 1..right).map(|x| buf[(x, y)].symbol()).collect();
            let Some(word) = interior.split_whitespace().next() else {
                continue;
            };
            let mut chars = word.chars();
            let sentence_case = chars.next().is_some_and(|c| c.is_ascii_uppercase())
                && chars.next().is_some_and(|c| c.is_ascii_lowercase());
            assert!(
                !sentence_case,
                "{}: a modal body row opens in sentence case: {interior:?}",
                case.name
            );
        }
    }
}

fn ctrl_c() -> ratatui::crossterm::event::KeyEvent {
    ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Char('c'),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    )
}

fn press(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
    crate::testutil::key(code)
}

/// The settled footer row with its 1-cell inset kept.
fn footer_row(app: &App) -> String {
    settled_rows(app, 80, 24)
        .last()
        .expect("the footer row")
        .trim_end()
        .to_string()
}

/// ctrl+c arms a quit that only a second ctrl+c completes: the first press
/// only raises ` ! press ⌃c again to quit`; `q` neither completes it nor is
/// completed by it, each running its own job instead.
#[test]
fn ctrl_c_arms_a_quit_only_a_second_ctrl_c_completes() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    use std::sync::atomic::Ordering;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = bare_app();
    app.tab = Tab::Usage;
    handle_key(&mut app, ctrl_c());
    assert!(!app.quit, "the first ctrl+c only arms");
    assert_eq!(footer_row(&app), " ! press ⌃c again to quit");
    handle_key(&mut app, ctrl_c());
    assert!(app.quit, "the second ctrl+c quits");
    assert!(app.shutting_down.load(Ordering::SeqCst));

    let mut app = bare_app();
    app.tab = Tab::Usage;
    handle_key(&mut app, ctrl_c());
    handle_key(&mut app, press(KeyCode::Char('q')));
    assert!(!app.quit, "q does not complete a ctrl+c arm");
    assert_eq!(
        footer_row(&app),
        " ! press q again to quit",
        "q runs its own job: at the top level it arms its own quit"
    );
    handle_key(&mut app, ctrl_c());
    assert!(!app.quit, "ctrl+c does not complete a q arm");
    assert_eq!(footer_row(&app), " ! press ⌃c again to quit");

    let mut app = bare_app();
    app.tab = Tab::Usage;
    handle_key(&mut app, ctrl_c());
    handle_key(&mut app, press(KeyCode::Right));
    assert!(!app.quit);
    assert_eq!(app.tab, Tab::Usage.next(), "another key does its own job");
    assert!(
        !footer_row(&app).contains("again to quit"),
        "another key disarms the ctrl+c quit"
    );
    handle_key(&mut app, ctrl_c());
    assert!(!app.quit, "a disarmed quit re-arms, it does not complete");
}

/// The first ctrl+c over an open modal or an open edit only arms: the modal
/// stays open, the edit keeps its text, and the footer shows the alert; a
/// typed key disarms and still types; two ctrl+c presses then quit.
#[test]
fn ctrl_c_arms_over_a_modal_and_an_edit_without_touching_them() {
    use crate::tui::app::{
        ConfirmAction, ConfirmScroll, ConfirmState, InputState, NamePromptAction, NamePromptForm,
        handle_key,
    };
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = bare_app();
    app.modals.push(Modal::Confirm(ConfirmState {
        message: "rotate 'x'?".into(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RotateOne("x".into()),
    }));
    handle_key(&mut app, ctrl_c());
    assert!(!app.quit, "the first ctrl+c over a modal only arms");
    assert_eq!(app.modals.len(), 1, "the modal stays open");
    assert_eq!(footer_row(&app), " ! press ⌃c again to quit");
    handle_key(&mut app, ctrl_c());
    assert!(app.quit, "the second ctrl+c quits over a modal");

    let mut app = bare_app();
    app.modals.push(Modal::NamePrompt(NamePromptForm {
        input: InputState::new("a"),
        action: NamePromptAction::DuplicateProfile("src".into()),
    }));
    handle_key(&mut app, ctrl_c());
    assert!(!app.quit);
    let value = |app: &App| match app.modals.last() {
        Some(Modal::NamePrompt(form)) => form.input.value.clone(),
        _ => panic!("the name prompt stays open"),
    };
    assert_eq!(value(&app), "a", "the edit keeps its text");
    assert_eq!(footer_row(&app), " ! press ⌃c again to quit");
    handle_key(&mut app, press(KeyCode::Char('b')));
    assert_eq!(value(&app), "ab", "a typed key still types");
    assert!(
        !footer_row(&app).contains("again to quit"),
        "a typed key disarms the quit"
    );
    handle_key(&mut app, ctrl_c());
    assert!(!app.quit, "the quit re-arms after the disarm");
    handle_key(&mut app, ctrl_c());
    assert!(app.quit, "two presses quit from inside an edit");
}

/// ctrl+c's arm alert is visible under a claude and a codex login line: the
/// login keeps running, but the one footer row shows the quit prompt, never
/// the login lead.
#[test]
fn the_quit_alert_is_visible_under_a_login_line() {
    use crate::tui::app::{CodexLoginSession, LoginMethod, LoginStage, handle_key};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = bare_app();
    app.tab = Tab::Usage;
    app.login = Some(LoginSession {
        name: "fresh".into(),
        is_new: false,
        generation: 0,
        url: None,
        stage: LoginStage::WaitingBrowser,
        method: LoginMethod::Browser,
        paste: None,
        paste_field: None,
    });
    handle_key(&mut app, ctrl_c());
    assert!(!app.quit, "the first ctrl+c only arms");
    assert_eq!(
        footer_row(&app),
        " ! press ⌃c again to quit",
        "the alert outranks the claude login line"
    );

    let mut app = bare_app();
    app.tab = Tab::Usage;
    app.codex_login = Some(CodexLoginSession {
        name: "fresh".into(),
        generation: 0,
        url: None,
        cancel: crate::codex_login::LoginCancel::new(),
        is_new: false,
    });
    handle_key(&mut app, ctrl_c());
    assert!(!app.quit, "the first ctrl+c only arms");
    assert_eq!(
        footer_row(&app),
        " ! press ⌃c again to quit",
        "the alert outranks the codex login line"
    );
}

/// `x` disarms a quit like any other key: over a toast it pops the topmost
/// toast AND clears the arm (ctrl+c's and q's alike); with no toast it
/// dismisses the footer alert as before. The toast pops in both.
#[test]
fn x_over_a_toast_pops_and_disarms_the_quit() {
    use crate::tui::app::{ToastKind, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = bare_app();
    app.tab = Tab::Usage;
    handle_key(&mut app, ctrl_c());
    app.toast(ToastKind::Info, "saved");
    assert_eq!(app.toasts.len(), 1, "a toast is up");
    handle_key(&mut app, press(KeyCode::Char('x')));
    assert!(app.toasts.is_empty(), "x pops the toast");
    assert!(app.armed_quit.is_none(), "x disarms the ctrl+c arm");
    assert!(app.footer_alert.is_none(), "x clears the alert");
    handle_key(&mut app, ctrl_c());
    assert!(!app.quit, "ctrl+c after x re-arms, it does not quit");

    let mut app = bare_app();
    app.tab = Tab::Usage;
    handle_key(&mut app, press(KeyCode::Char('q')));
    app.toast(ToastKind::Info, "saved");
    assert_eq!(app.toasts.len(), 1, "a toast is up over a q arm");
    handle_key(&mut app, press(KeyCode::Char('x')));
    assert!(app.toasts.is_empty(), "x pops the toast over a q arm too");
    assert!(app.armed_quit.is_none(), "x disarms the q arm");
    assert!(app.footer_alert.is_none(), "x clears the q alert");
    handle_key(&mut app, press(KeyCode::Char('q')));
    assert!(!app.quit, "q after x re-arms, it does not quit");

    let mut app = bare_app();
    app.tab = Tab::Usage;
    handle_key(&mut app, ctrl_c());
    assert_eq!(footer_row(&app), " ! press ⌃c again to quit");
    handle_key(&mut app, press(KeyCode::Char('x')));
    assert!(
        app.armed_quit.is_none(),
        "x with no toast dismisses the alert"
    );
    assert!(
        app.footer_alert.is_none(),
        "x with no toast clears the alert"
    );
    assert!(
        !footer_row(&app).contains("again to quit"),
        "the alert is gone"
    );
    handle_key(&mut app, ctrl_c());
    assert!(!app.quit, "the quit re-arms after x");
}
