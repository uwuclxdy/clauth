use super::*;
use crate::profile::{AppConfig, AppState};
use crate::tui::app::{
    App, ConfigFocus, FallbackFocus, ServicesFocus, StatusFocus, TokenView, has_sub_focus,
};

fn empty_app(tab: Tab) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = tab;
    app
}

/// An `Acknowledge` confirm's single ` close ` button line renders on a
/// terminal narrower than the button (the old 3-span split panicked at
/// `unreachable!`).
#[test]
fn acknowledge_confirm_renders_on_narrow_terminals() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    for width in [13u16, 14u16] {
        let state = ConfirmState {
            message: "account in use".into(),
            detail: None,
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::Acknowledge,
        };
        let (_buf, rows, _origin) = render_confirm(&state, width, 12);
        assert!(
            rows.join("\n").contains("cl"),
            "the close button renders at width {width}: {}",
            rows.join("\n")
        );
    }
}

/// An overflowing confirm body scrolls one line with ↓ and one page with
/// PageDown while the buttons stay pinned; ←/→ still pick the button and Enter
/// on the default `cancel` closes without running the action.
#[test]
fn confirm_body_scrolls_and_the_default_cancel_still_wins() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState, Modal, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = empty_app(Tab::Overview);
    let detail = format!(
        "{} and the very last sentence is TAILMARK",
        "filler words ".repeat(40)
    );
    app.open_modal(Modal::Confirm(ConfirmState {
        message: "replace the stored credentials?".into(),
        detail: Some(detail),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RotateAll,
    }));
    let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let before = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(!before.contains("TAILMARK"), "the tail starts offscreen");
    // ↓ scrolls one line at a time; the tail is not one press away.
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let one_down = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(
        !one_down.contains("TAILMARK"),
        "one ↓ is not the whole body"
    );
    // ↓ to the very bottom of the body.
    let mut presses = 1usize;
    loop {
        let max = match app.modals.last() {
            Some(Modal::Confirm(state)) => state.scroll.max_scroll.get(),
            _ => panic!("confirm mounted"),
        };
        let offset = match app.modals.last() {
            Some(Modal::Confirm(state)) => state.scroll.offset,
            _ => panic!("confirm mounted"),
        };
        if offset >= max {
            break;
        }
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
        presses += 1;
        assert!(presses < 60, "↓ should reach the tail");
    }
    let bottom = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(bottom.contains("TAILMARK"), "↓ reaches the tail");
    assert!(presses > 1, "the tail needs more than one ↓ press");
    assert!(
        bottom.contains("cancel"),
        "the cancel button stays pinned while the body is scrolled"
    );
    // ↑ scrolls back one line: the tail line leaves the view.
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let up_one = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(!up_one.contains("TAILMARK"), "one ↑ hides the tail again");
    // PageDown reaches the tail too.
    for _ in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    }
    let paged = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(
        paged.contains("TAILMARK"),
        "PageDown reaches the tail: {paged}"
    );
    // ←/→ still move the button focus, and Enter on `cancel` just closes.
    handle_key(&mut app, crate::testutil::key(KeyCode::Right));
    assert!(
        matches!(app.modals.last(), Some(Modal::Confirm(state)) if state.choice),
        "right focuses the affirmative button"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Left));
    assert!(
        matches!(app.modals.last(), Some(Modal::Confirm(state)) if !state.choice),
        "left focuses cancel again"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(app.modals.is_empty(), "Enter on cancel dismisses the modal");
    assert!(
        app.toasts.is_empty(),
        "a cancelled rotate-all action ran nothing"
    );
}

/// The help modal pages by the viewport the renderer drew (viewport 4 at
/// 80x12 → a 3-line step), and repeated presses reach the glyphs legend.
#[test]
fn help_modal_pages_by_the_rendered_viewport() {
    use crate::tui::app::{Modal, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = empty_app(Tab::Overview);
    app.open_modal(Modal::Help);
    let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        app.help_max_scroll.get() > 0,
        "the help modal overflows a short terminal"
    );
    assert_eq!(
        app.help_viewport.get(),
        4,
        "the 80x12 help modal draws four visible rows"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(app.help_scroll, 3, "one page = viewport - 1 = 3 lines");
    for _ in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    }
    assert_eq!(
        app.help_scroll,
        app.help_max_scroll.get(),
        "repeated pages reach the end of the help modal"
    );
}

/// The shunt card's `check output` modal pages like the help modal: viewport 4
/// at 80x12 → one PageDown shows `line-03` as the first body row, and repeated
/// presses stop at the last line.
#[test]
fn check_output_modal_pages_by_the_rendered_viewport() {
    use crate::tui::app::{Modal, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = empty_app(Tab::Services);
    let body = (0..30)
        .map(|i| format!("line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.open_modal(Modal::CheckOutput(body));
    let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert_eq!(
        app.check_output_viewport.get(),
        4,
        "the 80x12 check output modal draws four visible rows"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(
        app.check_output_scroll, 3,
        "one page = viewport - 1 = 3 lines"
    );
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let first = rows
        .iter()
        .position(|row| row.contains("line-"))
        .expect("a body line renders");
    assert!(
        rows[first].contains("line-03"),
        "the old last visible line is the new first: {}",
        rows[first]
    );
    for _ in 0..20 {
        handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    }
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let end = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(
        end.contains("line-29"),
        "repeated pages reach the last line"
    );
}

/// The action menu pages its cursor by the rendered viewport, stopping at the
/// last item.
#[test]
fn action_menu_pages_its_cursor_and_stops_at_the_end() {
    use crate::tui::app::{ActionItem, ActionMenuAction, ActionMenuState, Modal, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = empty_app(Tab::Overview);
    let menu = ActionMenuState {
        items: (0..30)
            .map(|_| ActionItem {
                label: "reload stats",
                hotkey: None,
                action: ActionMenuAction::ReloadTokenStats,
            })
            .collect(),
        scoped_len: 0,
        context: None,
        row_identity: None,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    app.open_modal(Modal::ActionMenu(menu));
    let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let viewport = match app.modals.last() {
        Some(Modal::ActionMenu(state)) => state.viewport.get(),
        _ => panic!("action menu mounted"),
    };
    assert_eq!(viewport, 4, "the 60x12 menu draws four visible rows");
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    let cursor = match app.modals.last() {
        Some(Modal::ActionMenu(state)) => state.cursor,
        _ => panic!("action menu mounted"),
    };
    assert_eq!(cursor, 3, "the menu cursor pages by viewport - 1 = 3");
    for _ in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    }
    let last = match app.modals.last() {
        Some(Modal::ActionMenu(state)) => state.cursor,
        _ => panic!("action menu mounted"),
    };
    assert_eq!(last, 29, "PageDown stops at the last item");
}

#[test]
fn help_modal_key_rows_follow_usage_and_model_detail_focus() {
    use ratatui::{Terminal, backend::TestBackend};

    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut usage = app_on(
        Tab::Usage,
        vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("acct"),
        )],
    );
    let mut tokens = empty_app(Tab::Tokens);
    let empty_usage = empty_app(Tab::Usage);

    let usage_at_rest = app_on(
        Tab::Usage,
        vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("acct"),
        )],
    );
    usage.usage_detail_focus = StatusFocus::Detail;
    let dashboard = empty_app(Tab::Tokens);
    let mut model_selector = empty_app(Tab::Tokens);
    model_selector.token_stats = Some(crate::tokens::TokenStats {
        models: vec![crate::tokens::ModelTokens {
            model: "claude-opus-4-8".into(),
            input: 12,
            output: 8,
            ..Default::default()
        }],
        ..Default::default()
    });
    model_selector.token_view = TokenView::Models;
    let mut empty_model_selector = empty_app(Tab::Tokens);
    empty_model_selector.token_view = TokenView::Models;
    let mut populated_dashboard = empty_app(Tab::Tokens);
    populated_dashboard.token_stats = model_selector.token_stats.clone();
    tokens.token_stats = model_selector.token_stats.clone();
    tokens.token_view = TokenView::Models;
    tokens.model_detail_focus = StatusFocus::Detail;

    let cases: [(&str, &App, &str, &[&str]); 8] = [
        (
            "usage selector",
            &usage_at_rest,
            "USAGE",
            &[
                "│    ↑ ↓                 pick account to inspect",
                "│    ↵                   open account detail",
                "│    r                   refresh account",
                "│    n                   edit the account's note",
                "│    e                   toggle estimates",
                "│    p                   toggle pace marker",
            ],
        ),
        (
            "empty usage",
            &empty_usage,
            "USAGE",
            &[
                "│    ↑ ↓                 pick account to inspect",
                "│    e                   toggle estimates",
                "│    p                   toggle pace marker",
            ],
        ),
        (
            "usage detail",
            &usage,
            "USAGE",
            &[
                "│    ↑ ↓                 scroll account detail",
                "│    esc                 return to account selector",
                "│    ←                   return to account selector",
                "│    r                   refresh account",
                "│    n                   edit the account's note",
                "│    e                   toggle estimates",
                "│    p                   toggle pace marker",
            ],
        ),
        (
            "empty tokens dashboard",
            &dashboard,
            "TOKENS",
            &[
                "│    ↑ ↓                 move between cards",
                "│    pgup/pgdn           move a page of cards",
                "│    c                   count cache in token figures",
                "│    t                   cycle period · lifetime / daily / weekly / monthly",
                "│    r                   reload on-disk stats",
            ],
        ),
        (
            "populated tokens dashboard",
            &populated_dashboard,
            "TOKENS",
            &[
                "│    ↑ ↓                 move between cards",
                "│    pgup/pgdn           move a page of cards",
                "│    ↵                   open per-model breakdown",
                "│    c                   count cache in token figures",
                "│    t                   cycle period · lifetime / daily / weekly / monthly",
                "│    r                   reload on-disk stats",
            ],
        ),
        (
            "empty model selector",
            &empty_model_selector,
            "TOKENS",
            &[
                "│    ↑ ↓                 pick model",
                "│    esc                 back to dashboard",
                "│    c                   count cache in token figures",
                "│    t                   cycle period · lifetime / daily / weekly / monthly",
                "│    r                   reload on-disk stats",
            ],
        ),
        (
            "model selector",
            &model_selector,
            "TOKENS",
            &[
                "│    ↑ ↓                 pick model",
                "│    ↵                   open model detail",
                "│    esc                 back to dashboard",
                "│    c                   count cache in token figures",
                "│    t                   cycle period · lifetime / daily / weekly / monthly",
                "│    r                   reload on-disk stats",
            ],
        ),
        (
            "model detail",
            &tokens,
            "TOKENS",
            &[
                "│    ↑ ↓                 scroll model detail",
                "│    esc                 return to model selector",
                "│    ←                   return to model selector",
                "│    c                   count cache in token figures",
                "│    t                   cycle period · lifetime / daily / weekly / monthly",
                "│    r                   reload on-disk stats",
            ],
        ),
    ];
    for (name, app, section, expected) in cases {
        let mut term = Terminal::new(TestBackend::new(130, 60)).unwrap();
        term.draw(|f| draw_help(f, f.area(), app)).unwrap();
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let section_row = rows
            .iter()
            .position(|row| row.contains(&format!("  {section} ")))
            .unwrap_or_else(|| panic!("{name}: missing {section} section"));
        let actual: Vec<String> = rows[section_row + 2..section_row + 2 + expected.len()]
            .iter()
            .map(|row| {
                let left = row.find('│').expect("modal border");
                row[left..]
                    .trim_end_matches([' ', '│'])
                    .trim_end()
                    .to_owned()
            })
            .collect();
        let projected = TestBackend::with_lines(actual);
        projected.assert_buffer_lines(expected.iter().copied());
    }
}

#[test]
fn help_tabs_and_screen_sections_follow_descended_detail_focus() {
    use ratatui::{Terminal, backend::TestBackend};

    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut usage = app_on(
        Tab::Usage,
        vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("acct"),
        )],
    );
    usage.usage_detail_focus = StatusFocus::Detail;
    let mut models = empty_app(Tab::Tokens);
    models.token_stats = Some(crate::tokens::TokenStats {
        models: vec![crate::tokens::ModelTokens {
            model: "claude-opus-4-8".into(),
            input: 12,
            output: 8,
            ..Default::default()
        }],
        ..Default::default()
    });
    models.token_view = TokenView::Models;
    models.model_detail_focus = StatusFocus::Detail;

    let cases: [(&str, &App, &[&str]); 2] = [
        (
            "usage detail",
            &usage,
            &[
                "│  USAGE",
                "│",
                "│    ↑ ↓                 scroll account detail",
                "│    esc                 return to account selector",
                "│    ←                   return to account selector",
                "│    r                   refresh account",
                "│    n                   edit the account's note",
                "│    e                   toggle estimates",
                "│    p                   toggle pace marker",
            ],
        ),
        (
            "model detail",
            &models,
            &[
                "│  TOKENS",
                "│",
                "│    ↑ ↓                 scroll model detail",
                "│    esc                 return to model selector",
                "│    ←                   return to model selector",
                "│    c                   count cache in token figures",
                "│    t                   cycle period · lifetime / daily / weekly / monthly",
                "│    r                   reload on-disk stats",
            ],
        ),
    ];
    for (name, app, screen) in cases {
        let mut term = Terminal::new(TestBackend::new(130, 60)).unwrap();
        term.draw(|f| draw_help(f, f.area(), app)).unwrap();
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let tabs = rows
            .iter()
            .position(|row| row.contains("  TABS "))
            .expect("tabs section");
        let modal = rows
            .iter()
            .position(|row| row.contains("  THIS MODAL "))
            .expect("modal section");
        let screen_start = rows
            .iter()
            .position(|row| {
                row.contains(if app.tab == Tab::Usage {
                    "  USAGE "
                } else {
                    "  TOKENS "
                })
            })
            .expect("screen section");
        let screen_end = rows
            .iter()
            .position(|row| row.contains("  GLOBAL "))
            .expect("global section");
        let tabs_rows: Vec<String> = rows[tabs..modal]
            .iter()
            .map(|row| {
                row.chars()
                    .skip_while(|ch| *ch != '│')
                    .collect::<String>()
                    .trim_end()
                    .trim_end_matches('│')
                    .trim_end()
                    .to_owned()
            })
            .collect();
        let expected_tabs = [
            "│  TABS",
            "│",
            "│    ←                   return to selector",
            "│    →                   stays on detail",
            "│",
        ];
        assert_eq!(
            tabs_rows.len(),
            expected_tabs.len(),
            "{name}: entire TABS section"
        );
        let tabs_expected = TestBackend::with_lines(tabs_rows.iter().map(String::as_str));
        tabs_expected.assert_buffer_lines(expected_tabs);
        let screen_rows: Vec<String> = rows[screen_start..screen_end]
            .iter()
            .map(|row| {
                row.chars()
                    .skip_while(|ch| *ch != '│')
                    .collect::<String>()
                    .trim_end()
                    .trim_end_matches('│')
                    .trim_end()
                    .to_owned()
            })
            .collect();
        let screen_expected =
            TestBackend::with_lines(screen_rows[..screen.len()].iter().map(String::as_str));
        screen_expected.assert_buffer_lines(screen.iter().copied());
        assert_eq!(
            screen_rows.len(),
            screen.len() + 1,
            "{name}: only the section's trailing blank precedes global"
        );
        assert_eq!(screen_rows[screen.len()], "│");
        assert!(
            modal < screen_start,
            "{name}: this modal section precedes the screen"
        );
    }
}

#[test]
fn empty_tokens_hides_enter_in_dashboard_and_model_selector_footer() {
    use ratatui::{Terminal, backend::TestBackend};

    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut empty = empty_app(Tab::Tokens);
    let mut populated = empty_app(Tab::Tokens);
    populated.token_stats = Some(crate::tokens::TokenStats {
        models: vec![crate::tokens::ModelTokens {
            model: "claude-opus-4-8".into(),
            input: 12,
            output: 8,
            ..Default::default()
        }],
        ..Default::default()
    });
    for (name, app, expected) in [
        (
            "empty dashboard",
            &empty,
            " ←→ tabs   r reload   c count cache   t period   a actions   ? help   q quit ",
        ),
        (
            "populated dashboard",
            &populated,
            " ←→ tabs   ↵ models   r reload   c count cache   t period   a actions   ? help   q quit ",
        ),
    ] {
        let mut term = Terminal::new(TestBackend::new(100, 1)).unwrap();
        term.draw(|f| super::super::footer::draw(f, f.area(), app))
            .unwrap();
        let rendered = crate::testutil::buffer_rows(term.backend().buffer());
        let exact = TestBackend::with_lines([rendered[0].trim_end()]);
        exact.assert_buffer_lines([expected.trim_end()]);
        assert_eq!(rendered.len(), 1, "{name}: one footer row");
    }
    empty.token_view = TokenView::Models;
    populated.token_view = TokenView::Models;
    for (name, app, expected) in [
        (
            "empty model selector",
            &empty,
            " ←→ tabs   ↑↓ model   c count cache   t period   a actions   ? help   q back ",
        ),
        (
            "populated model selector",
            &populated,
            " ←→ tabs   ↑↓ model   ↵ detail   c count cache   t period   a actions   ? help   q back ",
        ),
    ] {
        let mut term = Terminal::new(TestBackend::new(100, 1)).unwrap();
        term.draw(|f| super::super::footer::draw(f, f.area(), app))
            .unwrap();
        let rendered = crate::testutil::buffer_rows(term.backend().buffer());
        let exact = TestBackend::with_lines([rendered[0].trim_end()]);
        exact.assert_buffer_lines([expected.trim_end()]);
        assert_eq!(rendered.len(), 1, "{name}: one footer row");
    }
}

/// Issue #15: a tab with a descend/ascend sub-focus screen (Setup's Actions
/// pane, Fallback's Detail pane, Status/Plugin's Detail pane, Tokens' Models
/// view) must document an `esc` row in its help-modal section, or a user who
/// descended into it has no listed way back.
///
/// Driven off `has_sub_focus` — the same predicate the `q` handler and footer
/// use to decide "back" vs "quit" — rather than a hardcoded tab list, so a
/// future tab wired into that predicate without a matching help row fails
/// here instead of shipping undocumented.
#[test]
fn every_sub_focus_tab_documents_esc_in_help() {
    let _home = crate::testutil::HomeSandbox::new();
    for tab in Tab::ALL {
        let mut app = empty_app(tab);
        // Drive every sub-focus field to its "descended" value; `has_sub_focus`
        // only reads the one that matches `app.tab`, so this is safe for all.
        app.config_focus = ConfigFocus::Actions;
        app.fallback_focus = FallbackFocus::Detail;
        app.status.focus = StatusFocus::Detail;
        app.services.focus = ServicesFocus::Detail;
        app.token_view = TokenView::Models;

        if !has_sub_focus(&app) {
            continue;
        }

        let rows = tab_specific_rows(&app);
        let has_esc_row = rows
            .iter()
            .flat_map(|(_, entries)| entries.iter())
            .any(|(key, _)| *key == "esc");
        assert!(
            has_esc_row,
            "tab {tab:?} has a sub-focus but no `esc` row in its help-modal section"
        );
    }
}

/// Pins a tab's `(key, description)` help-modal rows, flattened across
/// sections and in order, exactly — so editing a key, its description, or
/// reordering the rows reds here instead of drifting unnoticed. Flattening
/// drops section titles: every current tab documents exactly one, so nothing
/// is lost. Add another tab's row list to this loop by extending the call.
fn assert_tab_rows(tab: Tab, expected: &[(&str, &str)]) {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = empty_app(tab);
    if tab == Tab::Usage {
        app = app_on(
            tab,
            vec![crate::testutil::blank_profile(
                &crate::profile::ProfileName::from("acct"),
            )],
        );
    }
    if tab == Tab::Tokens {
        app.token_view = TokenView::Models;
    }
    let rows: Vec<(&str, &str)> = tab_specific_rows(&app)
        .iter()
        .flat_map(|(_, entries)| entries.iter().copied())
        .collect();
    assert_eq!(rows, expected, "{tab:?} help-modal row list drifted");
}

#[test]
fn services_help_names_diagnostic_paging_without_mislabeling_arrows() {
    assert_tab_rows(
        Tab::Services,
        &[
            (
                "↑ ↓",
                "pick row · scroll shunt detail · walk plugin problems · walk herdr fix + options · walk delegates",
            ),
            (
                "pgup/pgdn",
                "scroll a page · read plugin / herdr diagnostics · page delegates",
            ),
            ("↵", "open detail · activate · open fix"),
            ("space", "activate the focused herdr option"),
            ("+ / -", "step the tag refresh"),
            ("f", "next fix"),
            ("r", "re-run all checks"),
            ("esc", "back to the list · close the editor"),
        ],
    );
}

#[test]
fn fallback_tab_key_grammar_rows_pin_exact_order_and_copy() {
    assert_tab_rows(
        Tab::Fallback,
        &[
            ("↑ ↓", "move cursor / detail row"),
            ("shift ↑ ↓", "reorder to set priority"),
            (
                "↵",
                "open · edit threshold · edit weekly at · edit max spend · toggle gates / last resort · remove · add",
            ),
            ("+ / -", "step rotate at / weekly at by 5"),
            (
                "↵ on rotate at",
                concat!("type a value, ", key_lit!("↵"), " saves"),
            ),
            ("↵ on weekly at", "type a %, empty clears"),
            (
                "space on preferred days",
                "step never / weekdays / weekends / every day",
            ),
            (
                "↵ on preferred days",
                concat!(
                    "pick days: ",
                    key_lit!("← →"),
                    " walk · ",
                    key_lit!("space"),
                    " toggles and saves · ",
                    key_lit!("↵ esc q"),
                    " leave · ",
                    key_lit!("↑ ↓"),
                    " leave and move",
                ),
            ),
            ("esc", "back / cancel edit"),
        ],
    );
}

/// A terminal too short for the whole keymap used to clamp the modal's height
/// and drop the tail with nothing on screen saying so — the legend, and before
/// it the Fallback tab's own last rows, simply were not there and the modal
/// looked complete. The overflow now carries the contract's scrollbar and ↑↓
/// reaches the tail.
#[test]
fn the_help_modal_scrolls_its_overflow_instead_of_dropping_it() {
    let _home = crate::testutil::HomeSandbox::new();
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = empty_app(Tab::Overview);
    let mut term = Terminal::new(TestBackend::new(130, 30)).unwrap();

    term.draw(|f| draw_help(f, f.area(), &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let (left, right) = rows
        .iter()
        .find_map(|row| {
            let chars: Vec<char> = row.chars().collect();
            Some((
                chars.iter().position(|c| *c == '\u{256d}')?,
                chars.iter().position(|c| *c == '\u{256e}')?,
            ))
        })
        .expect("the help modal's top border");

    assert!(
        !rows.iter().any(|r| r.contains("stale data")),
        "this terminal must be too short for the legend, or the test proves nothing:\n{}",
        rows.join("\n")
    );
    // The bar lives in the right padding column, one cell in from the border.
    let bar: Vec<char> = rows
        .iter()
        .filter_map(|r| r.chars().nth(right - 2))
        .filter(|c| *c == '\u{2503}' || *c == '\u{250a}')
        .collect();
    assert!(
        bar.contains(&'\u{2503}') && bar.contains(&'\u{250a}'),
        "clipped content must show a thumb on a track, got {bar:?}"
    );

    // The render pass publishes the bound the key handler clamps against.
    let max = app.help_max_scroll.get();
    assert!(max > 0, "a clipped modal must report a scrollable range");

    app.help_scroll = max;
    term.draw(|f| draw_help(f, f.area(), &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let slice =
        |row: &String| -> String { row.chars().skip(left).take(right - left + 1).collect() };
    let tail = rows
        .iter()
        .position(|r| r.contains("stale data"))
        .unwrap_or_else(|| {
            panic!(
                "scrolled to the end, the last row shows:\n{}",
                rows.join("\n")
            )
        });
    // The thumb has reached the bottom of its track, and it sits in the right
    // padding column — the content keeps every cell it had before.
    assert_eq!(
        slice(&rows[tail]),
        "│    ⋯                   stale data                                     ┃ │",
    );
}

/// A terminal short enough that the modal's chrome eats every row draws no
/// content at all, so there is nothing to scroll. The published bound has to
/// say so: `total - viewport` with a zero viewport yields the whole line count,
/// which would hand the key handler an offset for a modal drawing nothing — and
/// leave `help_scroll` stranded there once the terminal grew back.
#[test]
fn a_modal_with_no_room_to_draw_publishes_no_scroll_range() {
    let _home = crate::testutil::HomeSandbox::new();
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = empty_app(Tab::Overview);
    // 8 rows: `h` clamps to `area.height - 4`, which the border and the modal's
    // vertical padding consume whole.
    for height in [2, 5, 8] {
        let mut term = Terminal::new(TestBackend::new(100, height)).unwrap();
        term.draw(|f| draw_help(f, f.area(), &app)).unwrap();
        assert_eq!(
            app.help_max_scroll.get(),
            0,
            "a modal drawing zero rows at {height} rows must report no range"
        );
    }
}

/// A modal that fits keeps its exact geometry: the scroll plumbing must be a
/// no-op below overflow, not a one-row shift or a bar stealing a column. The
/// tail is pinned as an exact row slice, the same shape the legend pin below
/// uses — a `contains` would pass on a row shifted a column or wearing a bar.
///
/// It also pins the THIS MODAL section that documents the scroll itself, whole:
/// the row cannot go in `global`, since `draw_help` drops any global row whose
/// key the current tab redefines and all eight tabs bind ↑↓ — and hanging it
/// off another section would put two ↑↓ rows with different senses a few lines
/// apart, which is what that filter exists to prevent.
#[test]
fn a_help_modal_that_fits_renders_without_a_scrollbar() {
    let _home = crate::testutil::HomeSandbox::new();
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = empty_app(Tab::Overview);
    let mut term = Terminal::new(TestBackend::new(130, 60)).unwrap();
    term.draw(|f| draw_help(f, f.area(), &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let (left, right) = rows
        .iter()
        .find_map(|row| {
            let chars: Vec<char> = row.chars().collect();
            Some((
                chars.iter().position(|c| *c == '\u{256d}')?,
                chars.iter().position(|c| *c == '\u{256e}')?,
            ))
        })
        .expect("the help modal's top border");
    let slice =
        |row: &String| -> String { row.chars().skip(left).take(right - left + 1).collect() };

    assert_eq!(
        app.help_max_scroll.get(),
        0,
        "the whole modal fits at 60 rows"
    );
    assert!(
        rows.iter().all(|r| !r.contains('\u{250a}')),
        "no overflow, no scrollbar track:\n{}",
        rows.join("\n")
    );

    // The section whole — its header, both rows, and its placement. A copy-only
    // pin would pass with the scroll row parked back under TABS, four lines from
    // that section's own ↑↓ row.
    let head = rows
        .iter()
        .position(|r| r.contains("THIS MODAL"))
        .unwrap_or_else(|| panic!("the modal documents its own keys:\n{}", rows.join("\n")));
    assert_eq!(
        rows[head..head + 4].iter().map(slice).collect::<Vec<_>>(),
        vec![
            "│  THIS MODAL                                                             │"
                .to_string(),
            "│                                                                         │"
                .to_string(),
            "│    ↑ ↓                 scroll                                           │"
                .to_string(),
            "│    esc · q · ?         close                                            │"
                .to_string(),
        ],
    );
    // At rest the switch keys live in GLOBAL, so there is no TABS section —
    // THIS MODAL opens the keymap. (A descended pane's TABS arrow-trap block
    // is pinned by `help_tabs_and_screen_sections_follow_descended_detail_focus`.)
    assert!(
        rows.iter().all(|r| !r.contains("TABS")),
        "no tabs section at rest:\n{}",
        rows.join("\n")
    );

    let tail = rows
        .iter()
        .position(|r| r.contains("stale data"))
        .unwrap_or_else(|| panic!("the tail renders in full:\n{}", rows.join("\n")));
    assert_eq!(
        slice(&rows[tail]),
        "│    ⋯                   stale data                                       │",
    );
}

/// The Setup section's rows, pinned for the same reason as the Fallback ones —
/// and specifically so the disable row's gate copy can't drift back off the
/// `live session` noun `config::row_hint` and `actions::disable_profile` state
/// the same gate in. It shipped reading `a session is open` against their
/// `has a live session`, one screen apart, with no test on either wording.
#[test]
fn setup_tab_key_grammar_rows_pin_exact_order_and_copy() {
    assert_tab_rows(
        Tab::Setup,
        &[
            ("↑ ↓", "pick account / + new, then a row"),
            ("↵", "open settings · edit field · flip toggle"),
            (
                "↵ on a field",
                concat!("edit inline; ", key_lit!("↵"), " again saves"),
            ),
            ("space", "cycle the model preset (model row)"),
            ("+ / -", "step alert at by 5"),
            (
                "env",
                concat!("+ add env · ", key_lit!("↵"), " edits a value"),
            ),
            (
                "a",
                "duplicate the account · save it as a preset · apply one",
            ),
            (
                "disable / enable",
                concat!(
                    key_lit!("↵"),
                    " arms disable, again confirms · enable is one press · inert while active or a live session is open",
                ),
            ),
            (
                "delete",
                concat!(key_lit!("↵"), " once to arm, again to confirm"),
            ),
            ("esc", "stop editing / back to account list"),
        ],
    );
}

/// The help modal's GLYPHS legend, rendered whole. It is the only place the
/// account surfaces' 1-cell marks are explained, and two of them carry two
/// meanings apiece split on HUE alone (`⊖` disabled/canceled, `⊘` aggregate/
/// scoped week), so the pin asserts each row's text AND its mark's color — a
/// legend that lost a hue would read as a duplicate entry and sail past a
/// text-only check.
///
/// Driven through `draw_help` rather than `glyph_rows`, so it pins what a user
/// actually sees: the section's placement, its alignment against the key rows,
/// and that nothing clipped it.
#[test]
fn the_help_modal_legend_names_every_marker_and_its_hue() {
    let _home = crate::testutil::HomeSandbox::new();
    use ratatui::backend::TestBackend;
    use ratatui::{Terminal, style::Color};

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = empty_app(Tab::Overview);
    let mut term = Terminal::new(TestBackend::new(130, 60)).unwrap();
    term.draw(|f| draw_help(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let rows = crate::testutil::buffer_rows(&buf);
    // Slice each row to the modal's own columns so the pin is the modal alone,
    // not its centering offset within the terminal.
    let (left, right) = rows
        .iter()
        .find_map(|row| {
            let chars: Vec<char> = row.chars().collect();
            Some((
                chars.iter().position(|c| *c == '\u{256d}')?,
                chars.iter().position(|c| *c == '\u{256e}')?,
            ))
        })
        .expect("the help modal's top border");
    let slice =
        |row: &String| -> String { row.chars().skip(left).take(right - left + 1).collect() };

    let head = rows
        .iter()
        .position(|r| r.contains("GLYPHS"))
        .unwrap_or_else(|| panic!("the legend renders:\n{}", rows.join("\n")));
    // The section header, its blank, and one row per mark.
    assert_eq!(
        rows[head..head + 18].iter().map(slice).collect::<Vec<_>>(),
        vec![
            "│  GLYPHS                                                                 │"
                .to_string(),
            "│                                                                         │"
                .to_string(),
            "│    ●                   the active account                               │"
                .to_string(),
            "│    ⇄                   a live session here follows the fallback chain   │"
                .to_string(),
            "│    ↲                   the chain switches to this account next          │"
                .to_string(),
            "│    ⊖                   disabled                                         │"
                .to_string(),
            "│    ⊖                   canceled                                         │"
                .to_string(),
            "│    ×                   auth broken                                      │"
                .to_string(),
            "│    ×                   key rejected                                     │"
                .to_string(),
            "│    ⊘                   weekly spent                                     │"
                .to_string(),
            "│    ⧗                   claude code blocked                              │"
                .to_string(),
            "│    $                   extra usage spent                                │"
                .to_string(),
            "│    ◔                   5h window spent                                  │"
                .to_string(),
            "│    ⊘                   one model's week spent, other models ok          │"
                .to_string(),
            "│    ~                   past the weekly switch line, still serving       │"
                .to_string(),
            "│    ⋯                   stale data                                       │"
                .to_string(),
            "│    ◆                   can't read usage                                 │"
                .to_string(),
            "│    f                   has a fix                                        │"
                .to_string(),
        ],
    );

    // Every mark's own hue, read off the rendered cell. `⊖` and `⊘` are the
    // whole point: same shape, different color, different meaning. `×` keeps
    // one color for both its meanings.
    let expected: [Color; 16] = [
        crate::tui::theme::accent_2_color(),
        crate::tui::theme::text_dim_color(),
        crate::tui::theme::text_faint_color(),
        crate::tui::theme::text_faint_color(),
        crate::tui::theme::danger_color(),
        crate::tui::theme::danger_color(),
        crate::tui::theme::danger_color(),
        crate::tui::theme::danger_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::text_faint_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::accent_color(),
    ];
    // `left + 5`: the modal border, its 2-cell padding, and the row's own
    // 2-space gutter all sit ahead of the mark.
    let glyph_x = left + 5;
    let stride = buf.area.width as usize;
    let got: Vec<Color> = (0..16)
        .map(|i| buf.content[(head + 2 + i) * stride + glyph_x].fg)
        .collect();
    assert_eq!(got, expected.to_vec());
}

/// Every row of the help modal spells its arrow runs spaced, `← →` and `↑ ↓`:
/// the compact `←→` / `↑↓` is a hint-bar-only concession. Read off the rendered
/// modal on every tab, so a new row anywhere in it is held to the same rule.
#[test]
fn the_help_modal_spaces_every_arrow_run() {
    let _home = crate::testutil::HomeSandbox::new();
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut compact = Vec::new();
    for tab in Tab::ALL {
        let app = empty_app(tab);
        let mut term = Terminal::new(TestBackend::new(160, 120)).unwrap();
        term.draw(|f| draw_help(f, f.area(), &app)).unwrap();
        assert_eq!(
            app.help_max_scroll.get(),
            0,
            "{tab:?}: the whole modal is on screen, so every row is read"
        );
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        compact.extend(
            rows.iter()
                .filter(|r| r.contains("↑↓") || r.contains("←→"))
                .map(|r| format!("{tab:?}: {}", r.trim())),
        );
    }
    assert!(
        compact.is_empty(),
        "compact arrow runs:\n{}",
        compact.join("\n")
    );
}

// ── action menu ─────────────────────────────────────────────────────────────

/// Draw the menu the current context builds and return the screen rows plus the
/// modal's own left/right border columns, so a test can slice it out.
fn render_action_menu(app: &App, width: u16, height: u16) -> (Vec<String>, usize, usize) {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let state = crate::tui::app::build_action_menu(app);
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| draw_action_menu(f, f.area(), &state))
        .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let (left, right) = rows
        .iter()
        .find_map(|row| {
            let chars: Vec<char> = row.chars().collect();
            Some((
                chars.iter().position(|c| *c == '\u{256d}')?,
                chars.iter().position(|c| *c == '\u{256e}')?,
            ))
        })
        .unwrap_or_else(|| panic!("the action menu's top border:\n{}", rows.join("\n")));
    (rows, left, right)
}

fn app_on(tab: Tab, profiles: Vec<crate::profile::Profile>) -> App {
    let names = profiles.iter().map(|p| p.name.clone()).collect();
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: names,
            ..AppState::default()
        },
        profiles,
    });
    app.tab = tab;
    app
}

/// The account-scoped half of the menu names its account in the title bar, and
/// a rule holds it off the tab-global half below. Pinned whole: the name in the
/// right border break, the rule's own row, and which items land on each side.
#[test]
fn the_action_menu_titles_its_scope_and_rules_off_the_global_group() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);

    let app = app_on(
        Tab::Overview,
        vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("acct"),
        )],
    );
    let (rows, left, right) = render_action_menu(&app, 60, 20);
    let slice =
        |row: &String| -> String { row.chars().skip(left).take(right - left + 1).collect() };
    let top = rows
        .iter()
        .position(|r| r.contains('\u{256d}'))
        .expect("the top border");

    assert_eq!(
        rows[top..top + 11].iter().map(slice).collect::<Vec<_>>(),
        vec![
            "╭─ ACTIONS ───────────── acct ─╮".to_string(),
            "│                              │".to_string(),
            "│  ❯ refresh usage          r  │".to_string(),
            "│    rotate access token    t  │".to_string(),
            "│    disable account        d  │".to_string(),
            "│  ──────────────────────────  │".to_string(),
            "│    refresh all accounts   f  │".to_string(),
            "│    new account            n  │".to_string(),
            "│    start daemon           s  │".to_string(),
            "│    start shunt               │".to_string(),
            "│                              │".to_string(),
        ],
    );
}

/// A one-group menu has nothing to separate and no account to name: no rule
/// row, and the title bar stays bare rather than claiming a scope the items
/// don't have.
#[test]
fn a_single_group_action_menu_draws_no_rule_and_names_no_account() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);

    let (rows, left, right) = render_action_menu(&app_on(Tab::Overview, Vec::new()), 60, 20);
    let slice =
        |row: &String| -> String { row.chars().skip(left).take(right - left + 1).collect() };
    let top = rows
        .iter()
        .position(|r| r.contains('\u{256d}'))
        .expect("the top border");

    assert_eq!(
        rows[top..top + 8].iter().map(slice).collect::<Vec<_>>(),
        vec![
            "╭─ ACTIONS ────────────────────╮".to_string(),
            "│                              │".to_string(),
            "│  ❯ refresh all accounts   f  │".to_string(),
            "│    new account            n  │".to_string(),
            "│    start daemon           s  │".to_string(),
            "│    start shunt            t  │".to_string(),
            "│                              │".to_string(),
            "╰──────────────────────────────╯".to_string(),
        ],
    );
}

/// A menu that is scoped end to end (the Setup tab, whose three actions all
/// work on the account being configured, while a daemon start or stop holds
/// the daemon verb back and the gateway offers no shunt verb) still names that
/// account, and still draws no rule — there is no second group to hold off.
#[test]
fn an_all_scoped_action_menu_names_its_account_without_a_rule() {
    use crate::tui::app::{ConfigFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);

    let mut app = app_on(
        Tab::Setup,
        vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("acct"),
        )],
    );
    app.profile_cursor.set(0);
    // ⏎ on the account list is what seeds the draft the menu titles itself with.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.config_focus, ConfigFocus::Actions);
    app.daemon_control_busy = true;
    // A stale daemon's `unobserved` gateway offers no shunt verb.
    app.daemon_health = crate::daemon::DaemonHealth::Stale;
    app.gateway_state = crate::daemon::gateway::GatewayState::Unobserved;

    let (rows, left, right) = render_action_menu(&app, 60, 20);
    let slice =
        |row: &String| -> String { row.chars().skip(left).take(right - left + 1).collect() };
    let top = rows
        .iter()
        .position(|r| r.contains('\u{256d}'))
        .expect("the top border");

    assert_eq!(
        rows[top..top + 7].iter().map(slice).collect::<Vec<_>>(),
        vec![
            "╭─ ACTIONS ────────── acct ─╮".to_string(),
            "│                           │".to_string(),
            "│  ❯ duplicate account   d  │".to_string(),
            "│    save as preset      s  │".to_string(),
            "│    apply preset        p  │".to_string(),
            "│                           │".to_string(),
            "╰───────────────────────────╯".to_string(),
        ],
    );
}

// ── AddChainCandidate confirm modal ─────────────────────────────────────────
// The `+ add` picker's mix-guard modal pins its body copy and the candidate
// name in the confirm button label, so editing the message, dropping the
// detail, or reverting the button to the generic `confirm` reds here.

#[test]
fn add_chain_candidate_modal_pins_body_and_named_confirm_button() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let state = ConfirmState {
        message: "mixing api-key and oauth accounts can leave sessions stuck on the \
                  api account."
            .into(),
        detail: Some("api → oauth switches may not work until cc restarts.".into()),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::AddChainCandidate("test_name".into()),
    };
    let mut term = Terminal::new(TestBackend::new(200, 20)).unwrap();
    term.draw(|f| draw_confirm(f, f.area(), &state)).unwrap();
    let screen: String = crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .map(|r| r + "\n")
        .collect();

    assert!(
        screen.contains(
            "mixing api-key and oauth accounts can leave sessions stuck on the api account"
        ),
        "message copy missing:\n{screen}"
    );
    assert!(
        screen.contains("api → oauth switches may not work until cc restarts"),
        "detail copy missing:\n{screen}"
    );
    assert!(
        screen.contains("add 'test_name'"),
        "confirm button label must name the candidate:\n{screen}"
    );
    assert!(
        !screen.contains(" confirm "),
        "the generic `confirm` label must not appear for AddChainCandidate:\n{screen}"
    );
}

/// The Usage help section documents the note key while an account exists; on
/// an empty roster the row is gone, so the shadow filter leaves the global
/// `n new account` row standing, matching the empty state's promise.
#[test]
fn the_usage_help_section_documents_the_note_key() {
    let _home = crate::testutil::HomeSandbox::new();
    let populated = app_on(
        Tab::Usage,
        vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("acct"),
        )],
    );
    let rows = tab_specific_rows(&populated);
    let usage: Vec<&(&str, &str)> = rows
        .iter()
        .flat_map(|(_, entries)| entries.iter())
        .collect();
    assert!(
        usage.iter().any(|(k, _)| *k == "n"),
        "usage section must document the n key, got {usage:?}"
    );
    assert!(
        usage
            .iter()
            .any(|(k, d)| *k == "n" && *d == "edit the account's note"),
        "n's usage copy is pinned, got {usage:?}"
    );

    let empty: Vec<(&str, &str)> = tab_specific_rows(&empty_app(Tab::Usage))
        .iter()
        .flat_map(|(_, entries)| entries.iter().copied())
        .collect();
    assert!(
        empty.iter().all(|(k, _)| *k != "n"),
        "an empty roster keeps n = new account, got {empty:?}"
    );
}

/// The Usage help rows derive per row from the same predicate set as the
/// footer: `r refresh account` + `n` on a claude row, `r re-read the pool
/// listing` on a pool row, neither on a codex row, `↵` on any item row (F5).
#[test]
fn the_usage_help_rows_derive_per_row() {
    use crate::gateway_admin::{PoolAccount, PoolAuth, PoolProvider};
    use crate::profile::ProfileName;
    use crate::tui::app::ShuntPool;
    use ratatui::crossterm::event::KeyCode;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1"]);
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
    let mut app = app_on(
        Tab::Usage,
        vec![crate::testutil::blank_profile(&ProfileName::from("cl1"))],
    );
    app.shunt_pool = ShuntPool::Read(vec![PoolProvider {
        name: "claude-main".to_string(),
        auth: PoolAuth::Claude,
        accounts: vec![pool_account("alpha")],
    }]);

    let rows = |app: &App| -> Vec<(&str, &str)> {
        tab_specific_rows(app)
            .iter()
            .flat_map(|(_, entries)| entries.iter().copied())
            .collect()
    };

    assert_eq!(
        rows(&app),
        vec![
            ("\u{2191} \u{2193}", "pick account to inspect"),
            ("\u{21b5}", "open account detail"),
            ("r", "refresh account"),
            ("n", "edit the account's note"),
            ("e", "toggle estimates"),
            ("p", "toggle pace marker"),
        ],
        "the claude row's help rows"
    );

    crate::tui::app::handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        rows(&app),
        vec![
            ("\u{2191} \u{2193}", "pick account to inspect"),
            ("\u{21b5}", "open account detail"),
            ("e", "toggle estimates"),
            ("p", "toggle pace marker"),
        ],
        "the codex row's help rows"
    );

    crate::tui::app::handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        rows(&app),
        vec![
            ("\u{2191} \u{2193}", "pick account to inspect"),
            ("\u{21b5}", "open account detail"),
            ("r", "re-read the pool listing"),
            ("e", "toggle estimates"),
            ("p", "toggle pace marker"),
        ],
        "the pool row's help rows"
    );
}

/// The Usage help's GLOBAL section shadows the keys the tab redefines: `r` on
/// every Usage frame, `n` on every non-empty roster (a codex and a pool row
/// alike), while the empty roster keeps the global `n new account` (G3/PL8).
/// Pinned by equality, driven through `draw_help` so the shadow filter itself
/// is the code under test.
#[test]
fn the_usage_help_global_section_shadows_r_and_n_per_row() {
    use crate::gateway_admin::{PoolAccount, PoolAuth, PoolProvider};
    use crate::profile::ProfileName;
    use crate::tui::app::{ShuntPool, handle_key};
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};

    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
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

    let global_section = |app: &App| -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(130, 60)).unwrap();
        term.draw(|f| draw_help(f, f.area(), app)).unwrap();
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let inner: Vec<String> = rows
            .iter()
            .filter(|r| {
                let t = r.trim();
                t.starts_with('│')
            })
            .map(|r| r.split('│').nth(1).unwrap().trim_end().to_string())
            .collect();
        let global = inner
            .iter()
            .position(|r| r.trim() == "GLOBAL")
            .expect("the global section");
        let glyphs = inner
            .iter()
            .position(|r| r.trim() == "GLYPHS")
            .expect("the glyphs section");
        inner[global..glyphs].to_vec()
    };

    let without_r_or_n = vec![
        "  GLOBAL",
        "",
        "    t                   rotate all tokens",
        "    d                   resolve credential divergence (when flagged)",
        "    ?                   toggle this help",
        "    a                   actions",
        "    x                   dismiss toast / alert",
        "    q                   back / quit",
        "    ← →                 switch screen",
        "    tab                 next screen",
        "    ⇧tab                previous screen",
        "    ⌥1-9                jump to tab",
        "    esc                 back within a sub-view (no-op at the top level)",
        "    pgup/pgdn           scroll a page",
        "    ⌃c                  quit from anywhere",
        "",
    ];
    let with_n_without_r = vec![
        "  GLOBAL",
        "",
        "    n                   new account",
        "    t                   rotate all tokens",
        "    d                   resolve credential divergence (when flagged)",
        "    ?                   toggle this help",
        "    a                   actions",
        "    x                   dismiss toast / alert",
        "    q                   back / quit",
        "    ← →                 switch screen",
        "    tab                 next screen",
        "    ⇧tab                previous screen",
        "    ⌥1-9                jump to tab",
        "    esc                 back within a sub-view (no-op at the top level)",
        "    pgup/pgdn           scroll a page",
        "    ⌃c                  quit from anywhere",
        "",
    ];

    // Asserted before the codex roster is written: an empty roster keeps the
    // global `n new account` (nothing redefines `n`).
    assert_eq!(
        global_section(&empty_app(Tab::Usage)),
        with_n_without_r,
        "the empty roster keeps n new account but drops r"
    );

    crate::testutil::write_codex_roster(&["cx1"]);
    let mut app = app_on(
        Tab::Usage,
        vec![crate::testutil::blank_profile(&ProfileName::from("cl1"))],
    );
    app.shunt_pool = ShuntPool::Read(vec![PoolProvider {
        name: "claude-main".to_string(),
        auth: PoolAuth::Claude,
        accounts: vec![pool_account("alpha")],
    }]);

    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        app.usage_codex_index(),
        Some(0),
        "the codex row is selected"
    );
    assert_eq!(
        global_section(&app),
        without_r_or_n,
        "the codex row's GLOBAL section drops r and n"
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        app.usage_pool_pair(),
        Some((0, 0)),
        "the pool row is selected"
    );
    assert_eq!(
        global_section(&app),
        without_r_or_n,
        "the pool row's GLOBAL section drops r and n"
    );
}

/// The shunt confirm buttons name their own verb (adopt / move / add), never
/// the generic `confirm` — the `AddChainCandidate` named-button extension.
#[test]
fn the_shunt_confirm_buttons_name_their_verb() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let label = |action: ConfirmAction| -> String {
        let state = ConfirmState {
            message: "m".into(),
            detail: None,
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: action,
        };
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| draw_confirm(f, f.area(), &state)).unwrap();
        crate::testutil::buffer_rows(term.backend().buffer()).join("\n")
    };

    for (action, verb) in [
        (
            ConfirmAction::AdoptConfig("/cfg/shunt.toml".into()),
            "adopt",
        ),
        (
            ConfirmAction::MoveStoresIn(crate::gateway::StoreMovePlan {
                moved: Vec::new(),
                kept: Vec::new(),
            }),
            "move",
        ),
        (ConfirmAction::AddAdminKey, "add"),
        (ConfirmAction::AddAdminTable, "add"),
    ] {
        let screen = label(action);
        assert!(
            screen.contains(&format!(" {verb} ")),
            "the `{verb}` button renders:\n{screen}"
        );
        assert!(
            !screen.contains(" confirm "),
            "the generic `confirm` label must not appear for the shunt actions:\n{screen}"
        );
    }
}

/// The `add admin table` confirm is destructive (DANGER) — the add restarts the
/// gateway — while adopt / move / add-key stay neutral. Read off the rendered
/// unfocused `add` button's fill, so a plant dropping `AddAdminTable` from the
/// destructive set reds here (the label-only test above would not).
#[test]
fn the_add_admin_table_confirm_button_is_dangerous() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let render = |action: ConfirmAction| -> (String, ratatui::buffer::Buffer) {
        let state = ConfirmState {
            message: "m".into(),
            detail: None,
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: action,
        };
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| draw_confirm(f, f.area(), &state)).unwrap();
        let buf = term.backend().buffer().clone();
        (crate::testutil::buffer_rows(&buf).join("\n"), buf)
    };

    let (screen, buf) = render(ConfirmAction::AddAdminTable);
    let stride = buf.area.width as usize;
    let mut seen = None;
    for y in 0..buf.area.height as usize {
        for x in 0..stride {
            let cell = &buf.content[y * stride + x];
            if cell.symbol() == "a"
                && buf.content.get(y * stride + x + 1).map(|c| c.symbol()) == Some("d")
                && buf.content.get(y * stride + x + 2).map(|c| c.symbol()) == Some("d")
            {
                seen = Some(cell.fg);
            }
        }
    }
    assert_eq!(
        seen,
        Some(crate::tui::theme::danger_color()),
        "the add-admin-table confirm button is DANGER unfocused:\n{screen}"
    );

    // A neutral shunt confirm (adopt) stays dim.
    let (screen, buf) = render(ConfirmAction::AdoptConfig("/cfg/shunt.toml".into()));
    let stride = buf.area.width as usize;
    let mut seen = None;
    for y in 0..buf.area.height as usize {
        for x in 0..stride {
            let cell = &buf.content[y * stride + x];
            if cell.symbol() == "a"
                && buf.content.get(y * stride + x + 1).map(|c| c.symbol()) == Some("d")
                && buf.content.get(y * stride + x + 2).map(|c| c.symbol()) == Some("o")
                && buf.content.get(y * stride + x + 3).map(|c| c.symbol()) == Some("p")
                && buf.content.get(y * stride + x + 4).map(|c| c.symbol()) == Some("t")
            {
                seen = Some(cell.fg);
            }
        }
    }
    assert_eq!(
        seen,
        Some(crate::tui::theme::text_dim_color()),
        "the adopt confirm button is neutral (dim):\n{screen}"
    );
}

/// The `remove upstream …?` confirm's affirmative button is labeled `remove`
/// (never a generic `confirm`) and is destructive (DANGER fg, like its `stop
/// shunt?` sibling); `cancel` stays the default-focused neutral inverse block.
#[test]
fn the_remove_entry_confirm_labels_and_marks_the_remove_button_dangerous() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState, ShuntEntryKey};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let state = ConfirmState {
        message: "remove upstream \"a\"?".into(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RemoveEntry {
            key: ShuntEntryKey::Upstream {
                name: "a".into(),
                occurrence: 0,
            },
            edit: Box::new(crate::gateway_edit::Edit::RemoveUpstream { name: "a".into() }),
        },
    };
    let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
    term.draw(|f| draw_confirm(f, f.area(), &state)).unwrap();
    let buf = term.backend().buffer().clone();
    let screen = crate::testutil::buffer_rows(&buf).join("\n");
    let stride = buf.area.width as usize;
    let mut seen = None;
    for y in 0..buf.area.height as usize {
        for x in 0..stride {
            let cell = &buf.content[y * stride + x];
            if cell.symbol() == "r"
                && buf.content.get(y * stride + x + 1).map(|c| c.symbol()) == Some("e")
                && buf.content.get(y * stride + x + 2).map(|c| c.symbol()) == Some("m")
                && buf.content.get(y * stride + x + 3).map(|c| c.symbol()) == Some("o")
                && buf.content.get(y * stride + x + 4).map(|c| c.symbol()) == Some("v")
                && buf.content.get(y * stride + x + 5).map(|c| c.symbol()) == Some("e")
            {
                seen = Some(cell.fg);
            }
        }
    }
    assert_eq!(
        seen,
        Some(crate::tui::theme::danger_color()),
        "the remove confirm button is DANGER unfocused:\n{screen}"
    );
    assert!(
        !screen.contains("confirm"),
        "the button is labeled `remove`, never a generic `confirm`:\n{screen}"
    );
}

/// The `require client tokens?` confirm's affirmative button is destructive
/// (DANGER) — it refuses every consumer that sends no token, the same blast
/// radius `AddAdminTable` is marked destructive for.
#[test]
fn the_require_confirm_button_is_dangerous() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let state = ConfirmState {
        message: "require client tokens?".into(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RequireClientTokens {
            profiles: String::new(),
        },
    };
    let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
    term.draw(|f| draw_confirm(f, f.area(), &state)).unwrap();
    let buf = term.backend().buffer().clone();
    let screen = crate::testutil::buffer_rows(&buf).join("\n");
    let stride = buf.area.width as usize;
    let mut seen = None;
    for y in 0..buf.area.height as usize {
        for x in 0..stride {
            let cell = &buf.content[y * stride + x];
            if cell.symbol() == "r"
                && buf.content.get(y * stride + x + 1).map(|c| c.symbol()) == Some("e")
                && buf.content.get(y * stride + x + 2).map(|c| c.symbol()) == Some("q")
            {
                seen = Some(cell.fg);
            }
        }
    }
    assert_eq!(
        seen,
        Some(crate::tui::theme::danger_color()),
        "the require confirm button is DANGER unfocused:\n{screen}"
    );
}

/// Draw `state` as the TUI draws a confirm on a `width`×`height` screen and
/// return the buffer plus the modal's rows, border to border.
fn render_confirm(
    state: &crate::tui::app::ConfirmState,
    width: u16,
    height: u16,
) -> (ratatui::buffer::Buffer, Vec<String>, (u16, u16)) {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| draw_confirm(f, f.area(), state)).unwrap();
    let buf = term.backend().buffer().clone();
    let rows = crate::testutil::buffer_rows(&buf);
    let top = rows
        .iter()
        .position(|r| r.contains('\u{256d}'))
        .unwrap_or_else(|| panic!("the confirm's top border:\n{}", rows.join("\n")));
    let bottom = rows
        .iter()
        .position(|r| r.contains('\u{2570}'))
        .expect("the confirm's bottom border");
    let left = rows[top].chars().position(|c| c == '\u{256d}').unwrap();
    let right = rows[top].chars().position(|c| c == '\u{256e}').unwrap();
    let modal = rows[top..=bottom]
        .iter()
        .map(|row| row.chars().skip(left).take(right - left + 1).collect())
        .collect();
    (
        buf,
        modal,
        (u16::try_from(left).unwrap(), u16::try_from(top).unwrap()),
    )
}

/// The cell at `(x, y)` of the modal, counted from its top-left border corner.
fn modal_cell(
    buf: &ratatui::buffer::Buffer,
    origin: (u16, u16),
    x: usize,
    y: usize,
) -> &ratatui::buffer::Cell {
    let stride = buf.area.width as usize;
    &buf.content[(origin.1 as usize + y) * stride + origin.0 as usize + x]
}

/// The shared daemon-start confirm at 80 columns: its exact rows, the message in
/// TEXT, both detail lines TEXT_DIM with the `clauth daemon` command span in
/// ACCENT (never bold), and the right-aligned buttons, `cancel` focused as the
/// inverse block and `start` neutral.
#[test]
fn the_daemon_start_confirm_renders_its_rows_and_spans() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};
    use crate::tui::theme;
    use ratatui::style::Modifier;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let state = ConfirmState {
        message: "start daemon?".into(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::StartDaemon,
    };
    let (buf, rows, origin) = render_confirm(&state, 80, 24);
    assert_eq!(
        rows,
        vec![
            "╭─ CONFIRM ────────────────────────────────────╮".to_string(),
            "│                                              │".to_string(),
            "│  start daemon?                               │".to_string(),
            "│  spawns the clauth daemon detached: clauth   │".to_string(),
            "│  daemon                                      │".to_string(),
            "│  logfile at ~/.clauth/daemon.log.            │".to_string(),
            "│                                              │".to_string(),
            "│                             cancel   start   │".to_string(),
            "│                                              │".to_string(),
            "╰──────────────────────────────────────────────╯".to_string(),
        ],
    );
    let fg = |x: usize, y: usize| modal_cell(&buf, origin, x, y).fg;
    // Content starts at column 3: the border plus two cells of padding.
    assert_eq!(fg(3, 2), theme::text_color(), "the message is TEXT");
    // The detail wraps between words at 42 cells: the dim prefix, then the
    // ACCENT command split across the wrap.
    for x in 3..38 {
        assert_eq!(
            fg(x, 3),
            theme::text_dim_color(),
            "the prefix is TEXT_DIM at {x}"
        );
    }
    for x in 38..44 {
        let cell = modal_cell(&buf, origin, x, 3);
        assert_eq!(
            cell.fg,
            theme::accent_color(),
            "the command is ACCENT at {x}"
        );
        assert!(!cell.modifier.contains(Modifier::BOLD), "never bold at {x}");
    }
    for x in 3..9 {
        let cell = modal_cell(&buf, origin, x, 4);
        assert_eq!(
            cell.fg,
            theme::accent_color(),
            "the wrapped command tail is ACCENT at {x}"
        );
    }
    for x in 3..35 {
        assert_eq!(
            fg(x, 5),
            theme::text_dim_color(),
            "the logfile line is TEXT_DIM at {x}"
        );
    }
    // ` cancel ` spans columns 29..37, ` start ` 38..45.
    for x in 29..37 {
        let cell = modal_cell(&buf, origin, x, 7);
        assert_eq!(
            (cell.fg, cell.bg),
            (theme::bg(), theme::text_color()),
            "cancel is the focused inverse block at {x}"
        );
    }
    for x in 38..45 {
        assert_eq!(fg(x, 7), theme::text_dim_color(), "start is neutral at {x}");
    }
}

/// `stop shunt?` with its count clause: the exact rows and a DANGER `stop`
/// beside the focused `cancel`.
#[test]
fn the_stop_shunt_confirm_renders_its_rows_and_a_danger_stop() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let state = ConfirmState {
        message: "stop shunt?".into(),
        detail: Some(
            "2 live sessions use it; in-flight requests drain, then the gateway stays off until the daemon restarts."
                .into(),
        ),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::StopShunt,
    };
    // 120 columns cap the modal at 72 (60%), so the 103-cell detail wraps.
    let (buf, rows, origin) = render_confirm(&state, 120, 24);
    assert_eq!(
        rows,
        vec![
            "╭─ CONFIRM ────────────────────────────────────────────────────────────╮".to_string(),
            "│                                                                      │".to_string(),
            "│  stop shunt?                                                         │".to_string(),
            "│  2 live sessions use it; in-flight requests drain, then the gateway  │".to_string(),
            "│  stays off until the daemon restarts.                                │".to_string(),
            "│                                                                      │".to_string(),
            "│                                                      cancel   stop   │".to_string(),
            "│                                                                      │".to_string(),
            "╰──────────────────────────────────────────────────────────────────────╯".to_string(),
        ],
    );
    // ` stop ` spans the last six inner columns of the button row, 63..69.
    for x in 63..69 {
        assert_eq!(
            modal_cell(&buf, origin, x, 6).fg,
            crate::tui::theme::danger_color(),
            "stop is DANGER unfocused at {x}"
        );
    }
}

#[test]
fn long_choice_modals_keep_the_selected_option_and_thumb_visible() {
    use crate::tui::app::{
        ActionItem, ActionMenuAction, ActionMenuState, DivergenceTargetForm, PresetPickerForm,
    };
    use ratatui::{Terminal, backend::TestBackend};

    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let render = |draw: &dyn Fn(&mut Frame<'_>, Rect)| {
        let mut term = Terminal::new(TestBackend::new(80, 14)).unwrap();
        term.draw(|f| draw(f, f.area())).unwrap();
        crate::testutil::buffer_rows(term.backend().buffer())
    };
    let targets = (0..24).map(|i| format!("account-{i:02}")).collect();
    let target = DivergenceTargetForm {
        targets,
        cursor: 24,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    let rows = render(&|f, area| draw_divergence_target(f, area, &target));
    assert!(
        rows.iter()
            .any(|row| row.contains("❯ overwrite 'account-23'")),
        "selected target hidden: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains('┃')),
        "target overflow has no thumb: {rows:?}"
    );

    let presets = (0..24)
        .map(|i| crate::presets::Preset {
            name: format!("preset-{i:02}"),
            base_url: None,
            models: Default::default(),
            builtin: false,
        })
        .collect();
    let preset = PresetPickerForm {
        target: "work".into(),
        presets,
        cursor: 23,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    let rows = render(&|f, area| draw_preset_picker(f, area, &preset));
    assert!(
        rows.iter().any(|row| row.contains("❯ preset-23")),
        "selected preset hidden: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains('┃')),
        "preset overflow has no thumb: {rows:?}"
    );
    let mut preset = preset;
    preset.cursor = 5;
    let before = render(&|f, area| draw_preset_picker(f, area, &preset));
    let before_y = before
        .iter()
        .position(|row| row.contains("❯ preset-05"))
        .unwrap();
    preset.cursor = 6;
    let rows = render(&|f, area| draw_preset_picker(f, area, &preset));
    let after_y = rows
        .iter()
        .position(|row| row.contains("❯ preset-06"))
        .unwrap();
    assert_eq!(
        after_y,
        before_y + 1,
        "an interior selection must move within its viewport: {rows:?}"
    );
    preset.cursor = 23;
    let _ = render(&|f, area| draw_preset_picker(f, area, &preset));
    preset.cursor = 0;
    let rows = render(&|f, area| draw_preset_picker(f, area, &preset));
    assert!(
        rows.iter().any(|row| row.contains("❯ preset-00")),
        "wrapped selection cannot return to first: {rows:?}"
    );

    let menu = ActionMenuState {
        items: (0..24)
            .map(|_| ActionItem {
                label: "reload stats",
                hotkey: Some('r'),
                action: ActionMenuAction::ReloadTokenStats,
            })
            .collect(),
        scoped_len: 0,
        context: None,
        row_identity: None,
        cursor: 23,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    let rows = render(&|f, area| draw_action_menu(f, area, &menu));
    assert!(
        rows.iter().any(|row| row.contains("❯ reload stats")),
        "selected action hidden: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains('┃')),
        "action overflow has no thumb: {rows:?}"
    );

    let mut menu = menu;
    menu.cursor = 5;
    let before = render(&|f, area| draw_action_menu(f, area, &menu));
    let before_y = before
        .iter()
        .position(|row| row.contains("❯ reload stats"))
        .unwrap();
    menu.cursor = 6;
    let rows = render(&|f, area| draw_action_menu(f, area, &menu));
    let after_y = rows
        .iter()
        .position(|row| row.contains("❯ reload stats"))
        .unwrap();
    assert_eq!(
        after_y,
        before_y + 1,
        "an interior action must move within its viewport: {rows:?}"
    );
    let split = ActionMenuState {
        scoped_len: 12,
        cursor: 12,
        ..menu
    };
    let rows = render(&|f, area| draw_action_menu(f, area, &split));
    assert!(
        rows.iter().any(|row| row.contains("❯ reload stats")),
        "first global action hidden: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains('─') && !row.contains('╭') && !row.contains('╰')),
        "group rule missing: {rows:?}"
    );
    let mut app = empty_app(Tab::Tokens);
    app.open_modal(Modal::ActionMenu(ActionMenuState {
        items: vec![
            ActionItem {
                label: "period: lifetime",
                hotkey: Some('l'),
                action: ActionMenuAction::TokensPeriodLifetime,
            },
            ActionItem {
                label: "period: daily",
                hotkey: Some('p'),
                action: ActionMenuAction::TokensPeriodDaily,
            },
        ],
        scoped_len: 0,
        context: None,
        row_identity: None,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    }));
    crate::tui::app::handle_key(
        &mut app,
        crate::testutil::key(ratatui::crossterm::event::KeyCode::Char('p')),
    );
    assert!(app.modals.is_empty());
    assert_eq!(app.token_period, crate::tui::app::TokenPeriod::Daily);

    let narrow = ActionMenuState {
        items: vec![ActionItem {
            label: "reload stats",
            hotkey: Some('r'),
            action: ActionMenuAction::ReloadTokenStats,
        }],
        scoped_len: 0,
        context: None,
        row_identity: None,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    let mut term = Terminal::new(TestBackend::new(20, 14)).unwrap();
    term.draw(|f| draw_action_menu(f, f.area(), &narrow))
        .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        rows.iter().any(|row| row.contains("❯ rel…   r")),
        "narrow menu hid its assigned hotkey or label ellipsis: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains('┃')),
        "fitting menu drew a thumb: {rows:?}"
    );
    let env = crate::tui::app::EnvCollisionForm {
        profile: "work".into(),
        key: "ENDPOINT".into(),
        reason: "a very long setting name that wraps across the narrow modal content field".into(),
        existing_idx: None,
        cursor: 2,
        scroll: std::cell::Cell::new(0),
        reviewing_overwrite: false,
        confirm_overwrite: false,
        max_scroll: std::cell::Cell::new(0),
        max_scroll_stamp: std::cell::Cell::new(ScrollStamp::default()),
        viewport: std::cell::Cell::new(0),
        overwrite_needs_review: std::cell::Cell::new(false),
    };
    let rows = render(&|f, area| draw_env_collision(f, area, &env));
    assert!(
        rows.iter().any(|row| row.contains("❯ cancel")),
        "selected env choice hidden: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("back out, no change")),
        "selected choice detail hidden: {rows:?}"
    );
    let tall_env = crate::tui::app::EnvCollisionForm {
        reason: "your ~/.claude/settings.json".into(),
        cursor: 0,
        ..env
    };
    let mut narrow = Terminal::new(TestBackend::new(20, 14)).unwrap();
    narrow
        .draw(|f| draw_env_collision(f, f.area(), &tall_env))
        .unwrap();
    let rows = crate::testutil::buffer_rows(narrow.backend().buffer());
    assert!(
        rows.iter().any(|row| row.contains("❯ add the")),
        "selected overwrite action hidden: {rows:?}"
    );
    // The selected choice may exceed this viewport; its confirmation must carry the full consequence.
    let mut app = empty_app(Tab::Config);
    app.modals
        .push(crate::tui::app::Modal::EnvCollision(tall_env));
    let press = |app: &mut App, code| crate::tui::app::handle_key(app, crate::testutil::key(code));
    press(&mut app, ratatui::crossterm::event::KeyCode::Enter);
    let confirm = app
        .modals
        .last()
        .expect("overwrite needs a second confirmation");
    narrow
        .draw(|f| super::draw(f, f.area(), &app, confirm, &[]))
        .unwrap();
    let rows = crate::testutil::buffer_rows(narrow.backend().buffer());
    let buttons: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            let inside = row.split_once('│')?.1.split_once('│')?.0;
            matches!(inside.trim(), "cancel" | "add").then(|| inside.trim().to_owned())
        })
        .collect();
    assert_eq!(
        buttons,
        ["cancel", "add"],
        "both decisions stay visible before scrolling"
    );
    press(&mut app, ratatui::crossterm::event::KeyCode::Enter);
    assert!(
        matches!(app.modals.last(), Some(crate::tui::app::Modal::EnvCollision(form)) if !form.reviewing_overwrite),
        "cancel must return to the choice list"
    );
    press(&mut app, ratatui::crossterm::event::KeyCode::Enter);
    let confirm = app.modals.last().unwrap();
    narrow
        .draw(|f| super::draw(f, f.area(), &app, confirm, &[]))
        .unwrap();
    for _ in 0..20 {
        press(&mut app, ratatui::crossterm::event::KeyCode::Down);
    }
    let confirm = app.modals.last().unwrap();
    narrow
        .draw(|f| super::draw(f, f.area(), &app, confirm, &[]))
        .unwrap();
    let rows = crate::testutil::buffer_rows(narrow.backend().buffer());
    assert!(
        rows.iter().any(|row| row.contains("~/.claude"))
            && rows.iter().any(|row| row.contains("settings.j"))
            && rows.iter().any(|row| row.contains("son")),
        "overwrite confirmation hid its consequence ending: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("cancel")),
        "overwrite confirmation hid its safe choice: {rows:?}"
    );
    press(&mut app, ratatui::crossterm::event::KeyCode::Esc);
    assert!(
        matches!(app.modals.last(), Some(crate::tui::app::Modal::EnvCollision(form)) if !form.reviewing_overwrite),
        "esc must return to the choice list"
    );
    let full = crate::tui::app::EnvCollisionForm {
        profile: "work".into(),
        key: "ENDPOINT".into(),
        reason: "your ~/.claude/settings.json".into(),
        existing_idx: None,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        reviewing_overwrite: true,
        confirm_overwrite: false,
        max_scroll: std::cell::Cell::new(0),
        max_scroll_stamp: std::cell::Cell::new(ScrollStamp::default()),
        viewport: std::cell::Cell::new(0),
        overwrite_needs_review: std::cell::Cell::new(true),
    };
    let mut wide = Terminal::new(TestBackend::new(130, 24)).unwrap();
    wide.draw(|f| draw_env_collision(f, f.area(), &full))
        .unwrap();
    let consequence: Vec<_> = crate::testutil::buffer_rows(wide.backend().buffer())
        .into_iter()
        .filter(|row| row.contains("this account's value overrides"))
        .map(|row| {
            row.split_once('│')
                .unwrap()
                .1
                .split_once('│')
                .unwrap()
                .0
                .trim()
                .to_owned()
        })
        .collect();
    assert_eq!(
        consequence,
        ["this account's value overrides your ~/.claude/settings.json"],
        "the whole interpolated consequence must remain intact"
    );
    let divergence = crate::tui::app::DivergenceForm {
        active: "work".into(),
        sibling: Some("other".into()),
        cursor: 3,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    let rows = render(&|f, area| draw_divergence(f, area, &divergence));
    assert!(
        rows.iter().any(|row| row.contains("❯ discard this login")),
        "selected divergence choice hidden: {rows:?}"
    );
}

/// The preset picker teaches its `d` in prose: ACCENT + bold, the rest dim.
#[test]
fn the_preset_picker_styles_its_delete_key() {
    use crate::tui::app::PresetPickerForm;
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let form = PresetPickerForm {
        target: "work".into(),
        presets: Vec::new(),
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| draw_preset_picker(f, f.area(), &form))
        .unwrap();
    crate::testutil::assert_prose_part(
        term.backend().buffer(),
        "d deletes a saved preset",
        "d",
        true,
    );
}

/// A wrapped option list pages by lines, never skipping an option the user
/// never saw: at 36 cols each of the 30 long names wraps to two rows, so one
/// PageDown must keep the old last visible option on screen.
#[test]
fn divergence_target_pages_lines_without_skipping_an_option() {
    use crate::tui::app::{DivergenceTargetForm, Modal, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = empty_app(Tab::Overview);
    let targets = (0..30)
        .map(|i| format!("wrap{i:02} {}", "x".repeat(50)))
        .collect();
    app.open_modal(Modal::DivergenceTarget(DivergenceTargetForm {
        targets,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    }));
    let mut term = Terminal::new(TestBackend::new(36, 20)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let before = crate::testutil::buffer_rows(term.backend().buffer());
    let last_visible = before
        .iter()
        .rev()
        .find(|r| r.contains("wrap"))
        .map(|r| {
            let start = r.find("wrap").unwrap();
            r[start..start + 6].to_string()
        })
        .expect("a visible option row");
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    let cursor = match app.modals.last() {
        Some(Modal::DivergenceTarget(form)) => form.cursor,
        _ => panic!("picker still mounted"),
    };
    assert!(cursor > 0, "PageDown moves the cursor forward");
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let after = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        after.iter().any(|r| r.contains(&last_visible)),
        "the old last visible option stays visible after PageDown"
    );
}

/// The action menu's published row starts account for the rule line between
/// the scoped and global groups, so a page across the rule lands on the right
/// row.
#[test]
fn action_menu_publishes_starts_that_count_the_group_rule() {
    use crate::tui::app::{ActionItem, ActionMenuAction, ActionMenuState};
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let menu = ActionMenuState {
        items: (0..30)
            .map(|_| ActionItem {
                label: "reload stats",
                hotkey: None,
                action: ActionMenuAction::ReloadTokenStats,
            })
            .collect(),
        scoped_len: 12,
        context: None,
        row_identity: None,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| draw_action_menu(f, f.area(), &menu)).unwrap();
    let starts = menu.starts.borrow();
    assert_eq!(starts[11], 11, "the last scoped item sits on row 11");
    assert_eq!(
        starts[12], 13,
        "the first global item sits one row down, below the rule"
    );
    assert_eq!(
        menu.total.get(),
        31,
        "the rule line counts toward the total"
    );
}

/// PageDown across the group rule lands one row short of the identity step,
/// because the rule line sits between the groups and every row below it is one
/// line further down.
#[test]
fn action_menu_pages_across_the_rule_line() {
    use crate::tui::app::{ActionItem, ActionMenuAction, ActionMenuState, Modal, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = empty_app(Tab::Overview);
    let menu = ActionMenuState {
        items: (0..30)
            .map(|_| ActionItem {
                label: "reload stats",
                hotkey: None,
                action: ActionMenuAction::ReloadTokenStats,
            })
            .collect(),
        scoped_len: 12,
        context: None,
        row_identity: None,
        cursor: 11,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    app.open_modal(Modal::ActionMenu(menu));
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    let cursor = match app.modals.last() {
        Some(Modal::ActionMenu(state)) => state.cursor,
        _ => panic!("menu mounted"),
    };
    assert_eq!(
        cursor, 21,
        "the page step counts the rule line between groups"
    );
}

/// Hand-pinned page landings for the single-line selectors and option modals
/// the other tests don't reach: each literal is derived from the fixture at the
/// drawn size (one page = the pane's visible rows minus one), never from a
/// published viewport.
#[test]
fn selectors_and_modals_page_by_hand_pinned_cursors() {
    use crate::tokens::{ModelTokens, TokenStats};
    use crate::tui::app::{
        Check, DivergenceForm, DivergenceTargetForm, EnvCollisionForm, Health, Modal,
        PresetPickerForm, Tab, TokenView, handle_key,
    };
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let profiles: Vec<crate::profile::Profile> = (0..25)
        .map(|i| {
            crate::testutil::blank_profile(&crate::profile::ProfileName::from(format!(
                "acct-{i:02}"
            )))
        })
        .collect();

    // Tokens models selector: 25 single-line rows at 80x24, page = 17.
    let mut app = app_on(Tab::Tokens, vec![]);
    app.token_stats = Some(TokenStats {
        models: (0..25)
            .map(|i| ModelTokens {
                model: format!("claude-model-{i:02}"),
                input: 1,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    app.token_view = TokenView::Models;
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(app.token_model_cursor, 17, "tokens page");

    // Chain selector: 25 members + `+ add` = 26 rows at 80x24, page = 17.
    let mut app = app_on(Tab::Fallback, profiles.clone());
    app.config().state.fallback_chain = profiles.iter().map(|p| p.name.clone()).collect();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(app.chain_cursor, 17, "chain page");

    // Services selector: four rows all visible, page stops at the last.
    let mut app = empty_app(Tab::Services);
    app.services.checks = ["shunt", "delegates", "plugin", "herdr"]
        .into_iter()
        .map(|label| Check {
            label,
            health: Health::Ok,
            detail: vec![],
            fix: None,
            problems: vec![],
            shunt_focus: vec![],
            shunt_action_start: None,
        })
        .collect();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(
        app.services.cursor, 3,
        "services page stops at the last row"
    );

    // Preset picker: 24 rows at 60x14, page = 5.
    let mut app = empty_app(Tab::Overview);
    let presets = (0..24)
        .map(|i| crate::presets::Preset {
            name: format!("preset-{i:02}"),
            base_url: None,
            models: Default::default(),
            builtin: false,
        })
        .collect();
    app.open_modal(Modal::PresetPicker(PresetPickerForm {
        target: "work".into(),
        presets,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    }));
    let mut term = Terminal::new(TestBackend::new(60, 14)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert!(
        matches!(app.modals.last(), Some(Modal::PresetPicker(s)) if s.cursor == 5),
        "preset page"
    );

    // Divergence prompt: 4 rows at 60x14, page = 3.
    let mut app = empty_app(Tab::Overview);
    app.open_modal(Modal::Divergence(DivergenceForm {
        active: "work".into(),
        sibling: Some("other".into()),
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    }));
    let mut term = Terminal::new(TestBackend::new(60, 14)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert!(
        matches!(app.modals.last(), Some(Modal::Divergence(s)) if s.cursor == 3),
        "divergence page"
    );

    // Divergence target picker: 24 targets + `+ new` at 60x14, page = 5.
    let mut app = empty_app(Tab::Overview);
    let targets = (0..24).map(|i| format!("account-{i:02}")).collect();
    app.open_modal(Modal::DivergenceTarget(DivergenceTargetForm {
        targets,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    }));
    let mut term = Terminal::new(TestBackend::new(60, 14)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert!(
        matches!(app.modals.last(), Some(Modal::DivergenceTarget(s)) if s.cursor == 5),
        "target page"
    );

    // Env-collision review scrolls one page (viewport 2 -> step 1).
    let mut app = empty_app(Tab::Overview);
    app.open_modal(Modal::EnvCollision(EnvCollisionForm {
        profile: "work".into(),
        key: "ENDPOINT".into(),
        reason: "set by the base url field".into(),
        existing_idx: None,
        cursor: 2,
        scroll: std::cell::Cell::new(0),
        reviewing_overwrite: true,
        confirm_overwrite: false,
        max_scroll: std::cell::Cell::new(0),
        max_scroll_stamp: std::cell::Cell::new(ScrollStamp::default()),
        viewport: std::cell::Cell::new(0),
        overwrite_needs_review: std::cell::Cell::new(false),
    }));
    let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert!(
        matches!(app.modals.last(), Some(Modal::EnvCollision(s)) if s.scroll.get() == 1),
        "env review page"
    );
}

/// The action menu sizes its width from the widest label's cells, so a CJK
/// label renders whole instead of being cut by a char-derived modal width.
#[test]
fn the_action_menu_sizes_its_width_by_cells() {
    use crate::tui::app::{ActionItem, ActionMenuAction, ActionMenuState};
    use ratatui::{Frame, Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let menu = ActionMenuState {
        items: vec![ActionItem {
            label: "日本語の上流",
            hotkey: None,
            action: ActionMenuAction::ReloadTokenStats,
        }],
        scoped_len: 0,
        context: None,
        row_identity: None,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    let mut term = Terminal::new(TestBackend::new(80, 14)).unwrap();
    term.draw(|f: &mut Frame<'_>| draw_action_menu(f, f.area(), &menu))
        .unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        rows.iter().any(|row| row.contains('流')),
        "a CJK action label must render whole: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains('…')),
        "a CJK action label must not be needlessly cut: {rows:?}"
    );
}

/// The action menu's title measures the scope name by cells and follows the
/// panel meta rule: the name shows only while it leaves a 3-dash rule after
/// ` ACTIONS `, else it drops whole, and a modal narrower than its own title
/// ends the title in `…` inside the border.
#[test]
fn the_action_menu_sizes_its_title_by_cells() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = app_on(
        Tab::Overview,
        vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("cjk-日本語アカウント"),
        )],
    );
    let want: &[(u16, &str)] = &[
        (14, "╭─ ACTI… ╮"),
        (27, "╭─ ACTIONS ───────────╮"),
        (28, "╭─ ACTIONS ────────────╮"),
        (34, "╭─ ACTIONS ──────────────────╮"),
        (41, "╭─ ACTIONS ─────────────────────────╮"),
        (42, "╭─ ACTIONS ─── cjk-日 本 語 ア カ ウ ン ト  ─╮"),
        (45, "╭─ ACTIONS ─── cjk-日 本 語 ア カ ウ ン ト  ─╮"),
    ];
    for (w, expected) in want {
        let (rows, left, right) = render_action_menu(&app, *w, 20);
        let top = rows
            .iter()
            .position(|r| r.contains('\u{256d}'))
            .expect("the action menu's top border");
        let title: String = rows[top]
            .chars()
            .skip(left)
            .take(right - left + 1)
            .collect();
        assert_eq!(title, *expected, "action-menu title at {w} cols");
    }
}

/// Desktop pin: the action-menu title sizes by cells, so the CJK name does not
/// clip the `ACTIONS` scope to `ACTIO` — the pre-S9 clip.
#[test]
fn the_action_menu_title_desktop_rows_replace_the_pre_s9_clip() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = app_on(
        Tab::Overview,
        vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("cjk-日本語アカウント"),
        )],
    );
    for w in [80u16, 160] {
        let (rows, left, right) = render_action_menu(&app, w, 20);
        let top = rows
            .iter()
            .position(|r| r.contains('\u{256d}'))
            .expect("the action menu's top border");
        let title: String = rows[top]
            .chars()
            .skip(left)
            .take(right - left + 1)
            .collect();
        assert_eq!(
            title, "╭─ ACTIONS ─── cjk-日 本 語 ア カ ウ ン ト  ─╮",
            "action-menu title at {w} cols"
        );
    }
}

/// A 30-char ASCII account name (an email passes `validate_name_chars`) drops
/// from the action menu's title rather than cutting ` ACTIONS `, through the
/// width that would leave it a 2-dash rule, and shows whole behind a 3-dash
/// rule once the terminal has room.
#[test]
fn the_action_menu_drops_a_name_that_leaves_no_rule() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = app_on(
        Tab::Overview,
        vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("a.thirty.char.name@example.org"),
        )],
    );
    let want: &[(u16, &str)] = &[
        (34, "╭─ ACTIONS ──────────────────╮"),
        (51, "╭─ ACTIONS ───────────────────────────────────╮"),
        (52, "╭─ ACTIONS ─── a.thirty.char.name@example.org ─╮"),
        (80, "╭─ ACTIONS ─── a.thirty.char.name@example.org ─╮"),
    ];
    for (w, expected) in want {
        let (rows, left, right) = render_action_menu(&app, *w, 20);
        let top = rows
            .iter()
            .position(|r| r.contains('\u{256d}'))
            .expect("the action menu's top border");
        let title: String = rows[top]
            .chars()
            .skip(left)
            .take(right - left + 1)
            .collect();
        assert_eq!(title, *expected, "action-menu title at {w} cols");
    }
}

fn check_output_body_rows(term: &ratatui::Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
    let buf = term.backend().buffer();
    let area = *buf.area();
    let top = (0..area.height)
        .find(|&y| {
            let row: String = (0..area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("CHECK OUTPUT")
        })
        .expect("modal top border");
    let x = (0..area.width)
        .find(|&xx| buf[(xx, top)].symbol() == "C")
        .map(|c| c - 3)
        .expect("modal left corner");
    let width = (x..area.width)
        .find(|&xx| buf[(xx, top)].symbol() == "╮")
        .map(|xx| xx - x + 1)
        .expect("modal right corner");
    let bottom = (top + 1..area.height)
        .find(|&y| buf[(x, y)].symbol() == "╰")
        .expect("modal bottom border");
    (top + 1..bottom)
        .map(|y| (x..x + width).map(|xx| buf[(xx, y)].symbol()).collect())
        .collect()
}

/// The shunt card's `check output` modal splits a body line between graphemes
/// measured in display cells, so a wide CJK line chunks to rows that keep every
/// character (never a row ratatui then cuts past the inner width) and an emoji
/// grapheme stays whole. Pinned whole-row at 34/45/80.
#[test]
fn the_check_output_modal_chunks_body_lines_by_cells() {
    use crate::tui::app::{Modal, Tab};
    let _home = crate::testutil::HomeSandbox::new();
    let body = "日本語サービスの障害を調査中です\n⚠️ 🧭 emoji-line tail\nplain ascii line\n";
    let want: &[(u16, &[&str])] = &[
        (
            34,
            &[
                "│                            │",
                "│  日 本 語 サ ー ビ ス の 障 害 を 調   │",
                "│  査 中 で す                   │",
                "│  ⚠️  🧭  emoji-line tail     │",
                "│  plain ascii line          │",
                "│                            │",
            ],
        ),
        (
            45,
            &[
                "│                                    │",
                "│  日 本 語 サ ー ビ ス の 障 害 を 調 査 中 で す   │",
                "│  ⚠️  🧭  emoji-line tail             │",
                "│  plain ascii line                  │",
                "│                                    │",
            ],
        ),
        (
            80,
            &[
                "│                                    │",
                "│  日 本 語 サ ー ビ ス の 障 害 を 調 査 中 で す   │",
                "│  ⚠️  🧭  emoji-line tail             │",
                "│  plain ascii line                  │",
                "│                                    │",
            ],
        ),
    ];
    for (w, expected) in want {
        let mut app = empty_app(Tab::Services);
        app.open_modal(Modal::CheckOutput(body.to_string()));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(*w, 12)).unwrap();
        term.draw(|fr| crate::tui::render::draw(fr, &app)).unwrap();
        let got = check_output_body_rows(&term);
        assert_eq!(got, *expected, "check output modal body at {w} cols");
    }
}

/// An ASCII-only body chunks byte-identically to the old char budgeting: the
/// split happens at the same cell boundary (each ASCII char is one cell).
/// Pinned whole-row at 45/80 as the regression guard.
#[test]
fn the_check_output_modal_ascii_body_renders_unchanged() {
    use crate::tui::app::{Modal, Tab};
    let _home = crate::testutil::HomeSandbox::new();
    let body = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\nshort\n";
    let want: &[(u16, &[&str])] = &[
        (
            45,
            &[
                "│                                       │",
                "│  abcdefghijklmnopqrstuvwxyzABCDEFGHI  │",
                "│  JKLMNOPQRSTUVWXYZ0123456789          │",
                "│  short                                │",
                "│                                       │",
            ],
        ),
        (
            80,
            &[
                "│                                              │",
                "│  abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOP  │",
                "│  QRSTUVWXYZ0123456789                        │",
                "│  short                                       │",
                "│                                              │",
            ],
        ),
    ];
    for (w, expected) in want {
        let mut app = empty_app(Tab::Services);
        app.open_modal(Modal::CheckOutput(body.to_string()));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(*w, 12)).unwrap();
        term.draw(|fr| crate::tui::render::draw(fr, &app)).unwrap();
        let got = check_output_body_rows(&term);
        assert_eq!(got, *expected, "check output ascii body at {w} cols");
    }
}

/// A scrolled modal fits its title like a panel does: a short body leaves the
/// modal sized by its title, which renders whole with both insets, and a modal
/// narrower than its title ends the title in `…` inside the border.
#[test]
fn a_scrolled_modal_fits_its_title_by_cells() {
    use crate::tui::app::{Modal, Tab};
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    for (w, expected) in [(80u16, "╭─ CHECK OUTPUT ╮"), (14, "╭─ CHEC… ╮")] {
        let mut app = empty_app(Tab::Services);
        app.open_modal(Modal::CheckOutput("exit 1".to_string()));
        let mut term = Terminal::new(TestBackend::new(w, 12)).unwrap();
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let top = rows
            .iter()
            .find(|r| r.contains('\u{256d}') && r.contains("CHEC"))
            .expect("the check output modal's top border");
        let start = top.find("\u{256d}─ CHEC").expect("the modal's left corner");
        let end = start
            + top[start..]
                .find('\u{256e}')
                .expect("the modal's right corner");
        assert_eq!(
            &top[start..end + '\u{256e}'.len_utf8()],
            expected,
            "check output title at {w} cols"
        );
    }
}

// ── re-bind confirm render ───────────────────────────────────────────────────

/// Build a re-bind form for `profiles`, exactly as the startup path does.
fn rebind_form(profiles: Vec<crate::profile::Profile>) -> crate::tui::app::RebindForm {
    let config = AppConfig {
        state: AppState {
            profiles: profiles.iter().map(|p| p.name.clone()).collect(),
            ..AppState::default()
        },
        profiles,
    };
    let blocks = crate::presets::group_offers(crate::presets::rebind_offers(&config));
    let checked = blocks
        .iter()
        .map(|b| vec![true; b.accounts.len()])
        .collect();
    crate::tui::app::RebindForm {
        blocks,
        checked,
        cursor: 0,
        choice: false,
        scroll: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        digest: crate::presets::builtin_binds_digest(),
    }
}

fn rebind_render_form() -> crate::tui::app::RebindForm {
    use crate::profile::Profile;
    let mut ds1 = Profile::new(
        "DS1".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        None,
    );
    ds1.models.default = Some("deepseek-chat".to_string());
    rebind_form(vec![
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
        ds1,
    ])
}

fn render_rebind(form: &crate::tui::app::RebindForm, width: u16, height: u16) -> Vec<String> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| draw_rebind(f, f.area(), form)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let top = rows
        .iter()
        .position(|r| r.contains('\u{256d}'))
        .expect("the re-bind confirm's top border");
    let bottom = rows
        .iter()
        .position(|r| r.contains('\u{2570}'))
        .expect("the re-bind confirm's bottom border");
    let left = rows[top].chars().position(|c| c == '\u{256d}').unwrap();
    let right = rows[top].chars().position(|c| c == '\u{256e}').unwrap();
    rows[top..=bottom]
        .iter()
        .map(|row| row.chars().skip(left).take(right - left + 1).collect())
        .collect()
}

/// The re-bind confirm renders its grouped body through `draw_rebind` itself:
/// every row, the toggle glyphs, the cursor mark, and the button pair, pinned
/// by equality against the rendered buffer.
#[test]
fn the_rebind_confirm_renders_its_grouped_body() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let rows = render_rebind(&rebind_render_form(), 300, 40);
    assert_eq!(
        rows,
        [
            "╭─ CONFIRM ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮",
            "│                                                                                                                                                                       │",
            "│  move accounts onto the new preset models?                                                                                                                            │",
            "│  these accounts are on a built-in preset's endpoint. rows that are empty or hold an id an earlier release shipped take the preset's current value; ids you set stay.  │",
            "│                                                                                                                                                                       │",
            "│    default  (unset)        → opus                                                                                                                                     │",
            "│    opus     (unset)        → deepseek-v4-pro[1m]                                                                                                                      │",
            "│    sonnet   (unset)        → deepseek-flash[1m]                                                                                                                       │",
            "│    haiku    (unset)        → deepseek-flash[1m]                                                                                                                       │",
            "│    fable    (unset)        → deepseek-v4-pro[1m]                                                                                                                      │",
            "│  ❯ ─● D1                                                                                                                                                              │",
            "│    ─● D2                                                                                                                                                              │",
            "│                                                                                                                                                                       │",
            "│    default  deepseek-chat  → opus                                                                                                                                     │",
            "│    opus     (unset)        → deepseek-v4-pro[1m]                                                                                                                      │",
            "│    sonnet   (unset)        → deepseek-flash[1m]                                                                                                                       │",
            "│    haiku    (unset)        → deepseek-flash[1m]                                                                                                                       │",
            "│    fable    (unset)        → deepseek-v4-pro[1m]                                                                                                                      │",
            "│    ─● DS1                                                                                                                                                             │",
            "│                                                                                                                                                                       │",
            "│                                                                                                                                                                       │",
            "│                                                                                                                                                     not now   apply   │",
            "│                                                                                                                                                                       │",
            "╰───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯",
        ],
    );
}

/// On a terminal shorter than the list, the view follows the cursor: with the
/// cursor on the last account, its toggle line is on screen while the first
/// block's toggle has scrolled out.
#[test]
fn the_rebind_confirm_follows_its_cursor_on_a_short_terminal() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut form = rebind_render_form();
    form.cursor = form.account_count() - 1;
    let rows = render_rebind(&form, 100, 12);
    let joined = rows.join("\n");
    assert!(
        joined.contains("─● DS1"),
        "the cursor's account is on screen: {joined}"
    );
    assert!(
        !joined.contains("─● D1 "),
        "the first block scrolled out: {joined}"
    );
}

/// Returning to a block's first account (Down wraps the cursor back) keeps the
/// block head on screen: the confirm message and its `default` row, not just
/// the tail toggles. 20 accounts in one block at 100×24, the reviewer's R2-Q3
/// shape.
#[test]
fn the_rebind_confirm_wrap_back_shows_the_block_head() {
    use crate::profile::Profile;
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let profiles: Vec<Profile> = (0..20)
        .map(|i| {
            Profile::new(
                format!("acct{i:02}"),
                Some("https://api.deepseek.com/anthropic".to_string()),
                None,
            )
        })
        .collect();
    let mut form = rebind_form(profiles);
    // One block: every account is empty, so the change sets are identical.
    assert_eq!(form.blocks.len(), 1);
    assert_eq!(form.account_count(), 20);
    // Down × 20 wraps back to the first account; the follow re-runs per key the
    // way the event loop renders every frame.
    render_rebind(&form, 100, 24);
    for _ in 0..20 {
        form.cursor = (form.cursor + 1) % form.account_count();
        render_rebind(&form, 100, 24);
    }
    assert_eq!(form.cursor, 0, "Down × 20 wraps back to the first account");
    let joined = render_rebind(&form, 100, 24).join("\n");
    assert!(
        joined.contains("move accounts onto the new preset models?"),
        "the confirm message stays on screen: {joined}"
    );
    assert!(
        joined.contains("default  (unset)"),
        "the block's default row stays on screen: {joined}"
    );
}

/// A block taller than the viewport: the cursor's first-account toggle line
/// stays on screen even though the widened focus reaches back to the block head
/// (the toggle wins over the head).
#[test]
fn the_rebind_confirm_first_toggle_stays_visible_when_its_block_outgrows_the_viewport() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let form = rebind_render_form();
    let joined = render_rebind(&form, 100, 12).join("\n");
    assert!(
        joined.contains("─● D1"),
        "the first account's toggle is on screen: {joined}"
    );
}

/// Reach-back into a block after the first: `Up` into block 1's first account
/// keeps that block's change set on screen, not just its toggle.
#[test]
fn up_into_a_later_blocks_first_account_shows_its_rows() {
    use crate::profile::Profile;
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut profiles: Vec<Profile> = vec![
        Profile::new(
            "d0".to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            None,
        ),
        Profile::new(
            "d1".to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            None,
        ),
    ];
    for i in 0..15 {
        let mut p = Profile::new(
            format!("ds{i:02}"),
            Some("https://api.deepseek.com/anthropic".to_string()),
            None,
        );
        p.models.default = Some("deepseek-chat".to_string());
        profiles.push(p);
    }
    let mut form = rebind_form(profiles);
    assert_eq!(form.blocks.len(), 2);
    assert_eq!(form.account_count(), 17);
    render_rebind(&form, 100, 24);
    // Down to the last account, then Up into block 1's first account (ds00).
    for _ in 0..(form.account_count() - 1) {
        form.cursor = (form.cursor + 1) % form.account_count();
        render_rebind(&form, 100, 24);
    }
    assert_eq!(form.cursor, form.account_count() - 1, "on the last account");
    for _ in 0..(form.account_count() - 3) {
        form.cursor = (form.cursor + form.account_count() - 1) % form.account_count();
        render_rebind(&form, 100, 24);
    }
    assert_eq!(form.cursor, 2, "ds00 is block 1's first account");
    let joined = render_rebind(&form, 100, 24).join("\n");
    assert!(
        joined.contains("default  deepseek-chat  → opus"),
        "block 1's head is on screen: {joined}"
    );
}

/// The loss warning renders its WARNING title, its sentence-case body (one
/// line per sentence so a sentence never shares a row), and a right-aligned
/// `go back` / `discard` pair for both the Setup- and the Fallback-held body.
/// Exact rows at 80×24, the size the body once split `acc`/`ount.`.
#[test]
fn the_loss_warning_renders_its_title_body_and_buttons() {
    use crate::tui::app::{LossWarningDrop, LossWarningState, Modal};
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    for (drop, title, body) in [
        (
            LossWarningDrop::SetupDraft,
            "╭─ WARNING ───────────────────────────────────╮",
            vec![
                "",
                "  discard the unsaved edit?",
                "  there are unsaved edits on the setup tab.",
                "  fallback also edits this account.",
                "",
                "                         go back   discard",
                "",
            ],
        ),
        (
            LossWarningDrop::FallbackEdit,
            "╭─ WARNING ────────────────────────────────────╮",
            vec![
                "",
                "  discard the unsaved edit?",
                "  there are unsaved edits on the fallback",
                "  tab.",
                "  setup also edits this account.",
                "",
                "                          go back   discard",
                "",
            ],
        ),
    ] {
        let app = empty_app(Tab::Overview);
        let modal = Modal::LossWarning(LossWarningState {
            choice: false,
            target: Tab::Overview,
            drop,
        });
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, f.area(), &app, &modal, &[])).unwrap();
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let box_rows: Vec<&str> = rows
            .iter()
            .map(String::as_str)
            .filter(|r| {
                let t = r.trim();
                t.starts_with('╭') || t.starts_with('│') || t.starts_with('╰')
            })
            .collect();
        assert_eq!(box_rows[0].trim(), title, "the title row");
        let inner: Vec<String> = box_rows[1..box_rows.len() - 1]
            .iter()
            .map(|r| r.split('│').nth(1).unwrap().trim_end().to_string())
            .collect();
        assert_eq!(
            inner.iter().map(String::as_str).collect::<Vec<_>>(),
            body,
            "the body and button rows for {title}"
        );
    }
}

/// The loss warning's third body (the mint path displacing a suspended
/// `+ new` form) renders the same shell with its own second sentence.
/// Exact rows at 80×24; the width is the Setup body's, since both longest
/// sentences are 42 cells.
#[test]
fn the_new_account_draft_warning_renders_its_body() {
    use crate::tui::app::{LossWarningDrop, LossWarningState, Modal};
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = empty_app(Tab::Overview);
    let modal = Modal::LossWarning(LossWarningState {
        choice: false,
        target: Tab::Overview,
        drop: LossWarningDrop::NewAccountDraft { require: false },
    });
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| draw(f, f.area(), &app, &modal, &[])).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let box_rows: Vec<&str> = rows
        .iter()
        .map(String::as_str)
        .filter(|r| {
            let t = r.trim();
            t.starts_with('╭') || t.starts_with('│') || t.starts_with('╰')
        })
        .collect();
    assert_eq!(
        box_rows[0].trim(),
        "╭─ WARNING ───────────────────────────────────╮",
        "the title row"
    );
    let inner: Vec<String> = box_rows[1..box_rows.len() - 1]
        .iter()
        .map(|r| r.split('│').nth(1).unwrap().trim_end().to_string())
        .collect();
    assert_eq!(
        inner.iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "",
            "  discard the unsaved edit?",
            "  there are unsaved edits on the setup tab.",
            "  services also edits the new account form.",
            "",
            "                         go back   discard",
            "",
        ],
        "the body and button rows"
    );
}

/// The GLOBAL help section lists the screen keys after `q` (so after `?`/`a`
/// too) in the contract's order (`← →`, `tab`, `⇧tab`, `⌥1-9`), with clauth's
/// other rows left where they sat relative to `?`/`a`/`q`. Exact rows.
#[test]
fn the_help_global_section_names_the_switch_keys() {
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = empty_app(Tab::Overview);
    let mut term = Terminal::new(TestBackend::new(130, 60)).unwrap();
    term.draw(|f| draw_help(f, f.area(), &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let inner: Vec<String> = rows
        .iter()
        .filter(|r| {
            let t = r.trim();
            t.starts_with('│')
        })
        .map(|r| r.split('│').nth(1).unwrap().trim_end().to_string())
        .collect();
    let global = inner
        .iter()
        .position(|r| r.trim() == "GLOBAL")
        .expect("the global section");
    let glyphs = inner
        .iter()
        .position(|r| r.trim() == "GLYPHS")
        .expect("the glyphs section");
    let section: Vec<&str> = inner[global..glyphs].iter().map(String::as_str).collect();
    assert_eq!(
        section,
        vec![
            "  GLOBAL",
            "",
            "    n                   new account",
            "    r                   refresh usage now",
            "    t                   rotate all tokens",
            "    d                   resolve credential divergence (when flagged)",
            "    ?                   toggle this help",
            "    a                   actions",
            "    x                   dismiss toast / alert",
            "    q                   back / quit",
            "    ← →                 switch screen",
            "    tab                 next screen",
            "    ⇧tab                previous screen",
            "    ⌥1-9                jump to tab",
            "    esc                 back within a sub-view (no-op at the top level)",
            "    pgup/pgdn           scroll a page",
            "    ⌃c                  quit from anywhere",
            "",
        ]
    );
}

/// The descended editable pane's arrow-trap block names the inert keys —
/// `→ · tab · ⇧tab` all stay on the detail.
#[test]
fn the_help_editable_descended_block_names_the_inert_keys() {
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = empty_app(Tab::Setup);
    app.config_focus = ConfigFocus::Actions;
    let mut term = Terminal::new(TestBackend::new(120, 120)).unwrap();
    term.draw(|f| draw_help(f, f.area(), &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let tabs = rows
        .iter()
        .position(|r| r.contains("  TABS "))
        .expect("the tabs section");
    let block: Vec<String> = rows[tabs..tabs + 5]
        .iter()
        .map(|row| {
            row.chars()
                .skip_while(|ch| *ch != '│')
                .collect::<String>()
                .trim_end()
                .trim_end_matches('│')
                .trim_end()
                .to_owned()
        })
        .collect();
    let expected = TestBackend::with_lines(block.iter().map(String::as_str));
    expected.assert_buffer_lines([
        "│  TABS",
        "│",
        "│    ←                   return to selector",
        "│    → · tab · ⇧tab      stays on detail",
        "│",
    ]);
}

// ── modal sizing, button gap, word wrap ────────────────────────────────────

/// The modal's width in cells, read off its top border.
fn modal_width(rows: &[String]) -> u16 {
    let top = rows
        .iter()
        .find(|r| r.contains('\u{256d}'))
        .expect("modal top border");
    let left = top.chars().position(|c| c == '\u{256d}').unwrap();
    let right = top.chars().position(|c| c == '\u{256e}').unwrap();
    (right - left + 1) as u16
}

/// The modal's rows sliced to its own columns, border to border.
fn slice_modal(rows: &[String]) -> Vec<String> {
    let top = rows
        .iter()
        .position(|r| r.contains('\u{256d}'))
        .expect("modal top border");
    let bottom = rows
        .iter()
        .position(|r| r.contains('\u{2570}'))
        .expect("modal bottom border");
    let left = rows[top].chars().position(|c| c == '\u{256d}').unwrap();
    let right = rows[top].chars().position(|c| c == '\u{256e}').unwrap();
    rows[top..=bottom]
        .iter()
        .map(|r| r.chars().skip(left).take(right - left + 1).collect())
        .collect()
}

/// Draw one frame at `width`×`height` through `draw` and return the buffer rows.
fn render_rows(
    width: u16,
    height: u16,
    draw: &dyn Fn(&mut ratatui::Frame<'_>, ratatui::layout::Rect),
) -> Vec<String> {
    use ratatui::{Terminal, backend::TestBackend};
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| draw(f, f.area())).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
}

/// The button row's content, borders and alignment padding stripped: the last
/// non-blank interior row (the buttons sit pinned at the modal's bottom).
fn button_tail(rows: &[String]) -> String {
    let row = rows
        .iter()
        .rev()
        .find(|r| {
            let t = r.trim_matches('\u{2502}').trim();
            !t.is_empty() && !t.starts_with('\u{2570}') && !t.starts_with('\u{256d}')
        })
        .expect("button row");
    row.trim_matches('\u{2502}').trim().to_string()
}

/// The text of one line, spans concatenated.
fn line_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// from 80 columns up a modal caps at 60% of the terminal width
/// (integer floor: 80→48, 100→60, 160→96); below 80 it caps at
/// `terminal − 4`; a short modal still fits its content.
#[test]
fn the_modal_caps_at_sixty_percent_from_eighty_columns() {
    use crate::tui::app::{
        ActionItem, ActionMenuAction, ActionMenuState, ConfirmAction, ConfirmScroll, ConfirmState,
        EnvCollisionForm,
    };
    use ratatui::layout::Rect;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let shunt = ConfirmState {
        message: "stop shunt?".into(),
        detail: Some(
            "2 live sessions use it; in-flight requests drain, then the gateway stays off until the daemon restarts."
                .into(),
        ),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::StopShunt,
    };
    for (w, want) in [
        (80u16, 48u16),
        (81, 48),
        (99, 59),
        (100, 60),
        (160, 96),
        (79, 75),
        (45, 41),
    ] {
        let (_buf, rows, _origin) = render_confirm(&shunt, w, 24);
        let got = rows[0].chars().count() as u16;
        assert_eq!(got, want, "stop shunt? modal width at {w} cols");
    }

    let ack = ConfirmState {
        message: "account in use".into(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::Acknowledge,
    };
    let (_buf, rows, _origin) = render_confirm(&ack, 80, 24);
    assert_eq!(
        rows[0].chars().count(),
        20,
        "a short acknowledge stays content-sized at 80: {}",
        rows[0]
    );

    let env = EnvCollisionForm {
        profile: "work".into(),
        key: "ENDPOINT".into(),
        reason: "a very long setting name that wraps across the narrow modal content field".into(),
        existing_idx: None,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        reviewing_overwrite: false,
        confirm_overwrite: false,
        max_scroll: std::cell::Cell::new(0),
        max_scroll_stamp: std::cell::Cell::new(ScrollStamp::default()),
        viewport: std::cell::Cell::new(0),
        overwrite_needs_review: std::cell::Cell::new(false),
    };
    let rows = render_rows(80, 24, &|f: &mut ratatui::Frame<'_>, a: Rect| {
        draw_env_collision(f, a, &env)
    });
    assert_eq!(modal_width(&rows), 48, "env collision modal caps at 60%");
    let rows = render_rows(45, 24, &|f: &mut ratatui::Frame<'_>, a: Rect| {
        draw_env_collision(f, a, &env)
    });
    assert_eq!(
        modal_width(&rows),
        41,
        "env collision modal keeps width-4 below 80"
    );

    let menu = ActionMenuState {
        items: vec![ActionItem {
            label: "this is a very long action label that overflows sixty percent",
            hotkey: Some('x'),
            action: ActionMenuAction::ReloadTokenStats,
        }],
        scoped_len: 0,
        context: None,
        row_identity: None,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    };
    let rows = render_rows(80, 24, &|f: &mut ratatui::Frame<'_>, a: Rect| {
        draw_action_menu(f, a, &menu)
    });
    assert_eq!(modal_width(&rows), 48, "the action menu shares the 60% cap");
}

/// every button group keeps a 1-cell gap between buttons; the always-on
/// ` label ` insets and right alignment stay. Pinned by the exact button row.
#[test]
fn the_modal_button_groups_keep_a_one_cell_gap() {
    use crate::tui::app::{
        ConfirmAction, ConfirmScroll, ConfirmState, EnvCollisionForm, LossWarningDrop,
        LossWarningState, PresetApplyButton, PresetApplyForm, PresetTarget, Tab,
    };
    use ratatui::layout::Rect;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let shunt = ConfirmState {
        message: "stop shunt?".into(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::StopShunt,
    };
    let (_buf, rows, _origin) = render_confirm(&shunt, 80, 24);
    assert_eq!(
        button_tail(&rows),
        "cancel   stop",
        "two-button confirm gap"
    );

    let apply = PresetApplyForm {
        target: PresetTarget::NewAccount,
        preset: crate::presets::Preset {
            name: "p".into(),
            base_url: None,
            models: Default::default(),
            builtin: false,
        },
        message: "apply preset p?".into(),
        replaces: vec!["base url"],
        leaves: vec!["models"],
        focus: PresetApplyButton::Cancel,
        scroll: ConfirmScroll::default(),
    };
    let rows = slice_modal(&render_rows(
        80,
        24,
        &|f: &mut ratatui::Frame<'_>, a: Rect| draw_preset_apply(f, a, &apply),
    ));
    assert_eq!(
        button_tail(&rows),
        "cancel   keep them   clear them",
        "preset apply gap"
    );

    let env = EnvCollisionForm {
        profile: "work".into(),
        key: "ENDPOINT".into(),
        reason: "your ~/.claude/settings.json".into(),
        existing_idx: None,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        reviewing_overwrite: true,
        confirm_overwrite: false,
        max_scroll: std::cell::Cell::new(0),
        max_scroll_stamp: std::cell::Cell::new(ScrollStamp::default()),
        viewport: std::cell::Cell::new(0),
        overwrite_needs_review: std::cell::Cell::new(true),
    };
    let rows = slice_modal(&render_rows(
        80,
        24,
        &|f: &mut ratatui::Frame<'_>, a: Rect| draw_env_collision(f, a, &env),
    ));
    assert_eq!(button_tail(&rows), "cancel   add", "env collision gap");
    // The review group's pair also right-aligns to the inner-right edge, pinned
    // by the whole row: `│` + 2-cell padding + 28 alignment cells (42 - 14) +
    // ` cancel   add ` + 2-cell padding + `│`.
    let want = format!("│{}cancel   add{}│", " ".repeat(31), " ".repeat(3));
    let button_row = rows
        .iter()
        .rev()
        .find(|r| r.contains("cancel"))
        .expect("button row");
    assert_eq!(
        button_row.as_str(),
        want,
        "env collision review right-aligns the pair"
    );

    let loss = LossWarningState {
        choice: false,
        target: Tab::Overview,
        drop: LossWarningDrop::SetupDraft,
    };
    let rows = slice_modal(&render_rows(
        80,
        24,
        &|f: &mut ratatui::Frame<'_>, a: Rect| draw_loss_warning(f, a, &loss),
    ));
    assert_eq!(button_tail(&rows), "go back   discard", "loss warning gap");

    let rows = render_rebind(&rebind_render_form(), 80, 40);
    assert_eq!(button_tail(&rows), "not now   apply", "rebind gap");
}

/// `chunk_line` wraps at word boundaries, still splits a word
/// longer than the row by display cell between graphemes, measures wide glyphs
/// by cells, and keeps each span's style across the break.
#[test]
fn chunk_line_wraps_between_words_and_splits_long_words_by_cells() {
    use crate::tui::theme;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};

    let rows = chunk_line(Line::from("a b c d e"), 5);
    assert!(
        rows.iter().all(|r| r.width() <= 5),
        "no row wider than the inner width: {rows:?}"
    );
    let joined = rows.iter().map(line_text).collect::<Vec<_>>().join(" ");
    assert_eq!(
        joined, "a b c d e",
        "the source joins back whole, break spaces restored"
    );

    let rows = chunk_line(Line::from("abcdefghij"), 4);
    assert_eq!(
        rows.iter().map(line_text).collect::<Vec<_>>().concat(),
        "abcdefghij",
        "a long word splits by cell and loses nothing"
    );
    assert!(rows.iter().all(|r| r.width() <= 4));

    let rows = chunk_line(Line::from("日本語"), 4);
    assert_eq!(rows.len(), 2, "a wide glyph is measured by its cells");
    assert_eq!(rows[0].width(), 4);
    assert_eq!(rows[1].width(), 2);
    assert_eq!(
        rows.iter().map(line_text).collect::<Vec<_>>().concat(),
        "日本語"
    );

    let line = Line::from(vec![
        Span::styled("aa ", Style::default().fg(theme::accent_color())),
        Span::styled("bb cc dd", Style::default().fg(theme::text_dim_color())),
    ]);
    let rows = chunk_line(line, 5);
    assert_eq!(
        rows[0].spans.len(),
        2,
        "the break keeps the styled spans apart"
    );
    assert_eq!(rows[0].spans[0].content, "aa ");
    assert_eq!(rows[0].spans[0].style.fg, Some(theme::accent_color()));
    assert_eq!(rows[0].spans[1].content, "bb");
    assert_eq!(rows[0].spans[1].style.fg, Some(theme::text_dim_color()));
    assert_eq!(rows[1].spans[0].content, "cc dd");
    assert_eq!(rows[1].spans[0].style.fg, Some(theme::text_dim_color()));
}

/// The word-wrap invariants hold for every short input. Enumerating all strings
/// over `{a, b, space, tab, 漢, NBSP, NNBSP, figure space}` up to length 7 at
/// every width 1..=6: no row exceeds the width (a wider grapheme takes a row
/// of its own), the rows walk the tab-normalised source exactly (break
/// whitespace skipped only between rows, nothing else dropped, reordered or
/// duplicated), no row is empty, and no whitespace-only row follows a row that
/// carries content. The named probes and a multi-span probe then pin the exact
/// rows and each grapheme's span style.
#[test]
fn chunk_line_wraps_any_input_without_overrun_or_loss() {
    use crate::tui::theme;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};

    // Break points are whitespace minus the glue U+00A0/U+202F/U+2007; a tab is
    // one break space.
    fn is_break_space(c: char) -> bool {
        c.is_whitespace() && !matches!(c, '\u{a0}' | '\u{202f}' | '\u{2007}')
    }
    fn cell_w(c: char) -> usize {
        Span::raw(c.to_string()).width()
    }
    fn flatten_styled(line: &Line<'static>) -> Vec<(String, Style)> {
        let mut v = Vec::new();
        for span in &line.spans {
            for g in Span::raw(span.content.as_ref()).styled_graphemes(Style::default()) {
                v.push((g.symbol.to_string(), span.style));
            }
        }
        v
    }
    fn check_wrap_invariants(s: &str, w: usize, rows: &[Line<'static>]) {
        // (c) no empty row unless the source is empty.
        if !s.is_empty() {
            for (i, r) in rows.iter().enumerate() {
                assert!(!line_text(r).is_empty(), "empty row {i} for {s:?} at w={w}");
            }
        }
        // (a) a row may exceed `w` only as a single grapheme wider than `w`.
        for r in rows {
            if r.width() > w {
                assert!(
                    line_text(r).chars().count() == 1,
                    "{} cells in one row for {s:?} at w={w}: {r:?}",
                    r.width()
                );
            }
        }
        // (b) the rows walk the tab-normalised source exactly: break whitespace
        // is skipped only between rows, nothing else is dropped, reordered or
        // duplicated, and no row loses or gains whitespace mid-row.
        let source: String = s.chars().map(|c| if c == '\t' { ' ' } else { c }).collect();
        let mut it = source.chars().peekable();
        for (i, r) in rows.iter().enumerate() {
            let row_text = line_text(r);
            let leads_with_ws = row_text.chars().next().is_some_and(is_break_space);
            if i > 0 && !leads_with_ws {
                while it.peek().is_some_and(|&c| is_break_space(c)) {
                    it.next();
                }
            }
            for c in row_text.chars() {
                assert_eq!(
                    it.next(),
                    Some(c),
                    "mid-row content changed for {s:?} at w={w}: {rows:?}"
                );
            }
        }
        while it.peek().is_some_and(|&c| is_break_space(c)) {
            it.next();
        }
        assert!(
            it.next().is_none(),
            "content dropped for {s:?} at w={w}: {rows:?}"
        );
        // (c') a whitespace-only leading row is allowed only while the indent
        // leaves no room for the word's first grapheme (or the source line is
        // all whitespace); a whitespace-only row after content reds.
        if let Some(first) = s.chars().find(|&c| !is_break_space(c)) {
            let indent: usize = s
                .chars()
                .take_while(|&c| is_break_space(c))
                .map(cell_w)
                .sum();
            let first_ws_only = line_text(&rows[0]).chars().all(is_break_space);
            assert!(
                !first_ws_only || indent + cell_w(first) > w,
                "whitespace-only first row for {s:?} at w={w}: {rows:?}"
            );
        }
        let mut seen_content = false;
        for (i, r) in rows.iter().enumerate() {
            let ws_only = line_text(r).chars().all(is_break_space);
            assert!(
                !(ws_only && seen_content),
                "whitespace-only row {i} for {s:?} at w={w}: {rows:?}"
            );
            seen_content |= !ws_only;
        }
    }

    let alphabet = [
        'a', 'b', ' ', '\t', '漢', '\u{a0}', '\u{202f}', '\u{2007}', '\u{3000}',
    ];
    let mut checked = 0usize;
    for len in 0..=7usize {
        let total = alphabet.len().pow(len as u32);
        let mut buf = vec![' '; len];
        for mut n in 0..total {
            for slot in &mut buf {
                *slot = alphabet[n % alphabet.len()];
                n /= alphabet.len();
            }
            let s: String = buf.iter().collect();
            for w in 1..=6usize {
                check_wrap_invariants(&s, w, &chunk_line(Line::from(s.as_str()), w));
                checked += 1;
            }
            // (f) `w == 0` keeps the early return: the line, whole (tabs as
            // one space).
            let rows = chunk_line(Line::from(s.as_str()), 0);
            assert_eq!(rows.len(), 1, "w=0 keeps the line whole for {s:?}");
            let normalized: String = s.chars().map(|c| if c == '\t' { ' ' } else { c }).collect();
            assert_eq!(
                line_text(&rows[0]),
                normalized,
                "w=0 keeps the content for {s:?}"
            );
            checked += 1;
        }
    }
    assert!(checked > 2_000_000, "the sweep covered {checked} cases");

    // The named probes, pinned row for row.
    let probes: [(&str, usize, &[&str]); 18] = [
        (" abcd漢", 5, &[" abcd", "漢"]),
        (" abcdefgh", 5, &[" abcd", "efgh"]),
        ("  abcd", 5, &["  abc", "d"]),
        ("abcd   efgh", 5, &["abcd", "efgh"]),
        ("abcd  efgh   ", 5, &["abcd", "efgh "]),
        ("abcde ", 5, &["abcde"]),
        ("abcd ", 4, &["abcd"]),
        ("a ", 1, &["a"]),
        ("ab\u{3000}", 2, &["ab"]),
        ("     ", 4, &["    ", " "]),
        ("漢字", 1, &["漢", "字"]),
        ("abc\u{a0}def ghi", 5, &["abc\u{a0}d", "ef", "ghi"]),
        ("a\u{202f}b c", 3, &["a\u{202f}b", "c"]),
        ("a\u{2007}bcd e", 4, &["a\u{2007}bc", "d e"]),
        ("    ab", 4, &["    ", "ab"]),
        ("    漢b", 4, &["    ", "漢b"]),
        ("\u{3000}ab", 2, &["\u{3000}", "ab"]),
        (" \tab", 2, &["  ", "ab"]),
    ];
    for (s, w, want) in probes {
        let rows = chunk_line(Line::from(s), w);
        let got: Vec<String> = rows.iter().map(line_text).collect();
        let want: Vec<String> = want.iter().map(|x| (*x).to_string()).collect();
        assert_eq!(got, want, "probe {s:?} at w={w}");
    }

    // (d) every grapheme keeps the style of its source span across a split.
    let split = Line::from(vec![
        Span::styled("abcd", Style::default().fg(theme::accent_color())),
        Span::styled("efgh", Style::default().fg(theme::text_dim_color())),
    ]);
    let out: Vec<(String, Style)> = chunk_line(split.clone(), 3)
        .iter()
        .flat_map(flatten_styled)
        .collect();
    assert_eq!(
        out,
        flatten_styled(&split),
        "every grapheme keeps its span style across the split"
    );
}

/// A tab in a body line is one break space: it wraps like a space and never
/// vanishes or glues its neighbours.
#[test]
fn chunk_line_treats_a_tab_as_one_break_space() {
    use crate::tui::theme;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};

    let rows = chunk_line(Line::from("key\tvalue and more words here"), 10);
    assert_eq!(
        rows.iter().map(line_text).collect::<Vec<_>>(),
        ["key value", "and more", "words here"],
        "a tab wraps like a space"
    );

    // a leading tab is indent glue, exactly a leading space.
    let tab_indent = chunk_line(Line::from("\tab"), 2);
    let sp_indent = chunk_line(Line::from(" ab"), 2);
    assert_eq!(
        tab_indent.iter().map(line_text).collect::<Vec<_>>(),
        sp_indent.iter().map(line_text).collect::<Vec<_>>(),
        "a leading tab indents like a leading space"
    );

    // a tab-only line is all whitespace: it splits by cell like spaces.
    assert_eq!(
        chunk_line(Line::from("\t\t\t\t"), 3)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>(),
        ["   ", " "],
        "a tab-only line splits by cell"
    );

    // a tab at a span boundary is one space in the first span, breakable there.
    let line = Line::from(vec![
        Span::styled("tab\t", Style::default().fg(theme::accent_color())),
        Span::styled("ended", Style::default().fg(theme::text_dim_color())),
    ]);
    let rows = chunk_line(line, 5);
    assert_eq!(
        rows.iter().map(line_text).collect::<Vec<_>>(),
        ["tab", "ended"],
        "a tab ending a span breaks there"
    );
    assert_eq!(
        rows[0].spans[0].style.fg,
        Some(theme::accent_color()),
        "the break keeps the first span's style"
    );
    assert_eq!(
        rows[1].spans[0].style.fg,
        Some(theme::text_dim_color()),
        "the break keeps the second span's style"
    );
}

/// A check-output body keeps the tab between stderr fields as one space: the
/// field names never glue.
#[test]
fn the_check_output_modal_keeps_a_space_between_tab_separated_fields() {
    use crate::tui::app::{Modal, Tab};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = empty_app(Tab::Services);
    app.open_modal(Modal::CheckOutput("name\tvalue".to_string()));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
    term.draw(|fr| crate::tui::render::draw(fr, &app)).unwrap();
    let got = check_output_body_rows(&term);
    // content-fit: 10-cell body + 6 chrome = 16, but the title pins 17 -> a
    // 17-wide modal, 15 interior, 11 content cells; "name value" fills 10.
    let blank = format!("│{}│", " ".repeat(15));
    let want = vec![
        blank.clone(),
        format!("│  name value{}│", " ".repeat(3)),
        blank,
    ];
    assert_eq!(got, want, "the tab renders as one space at 80×30");
}

/// Trailing whitespace that would run past the row end is dropped, so a line
/// that ends in spaces draws no blank row before the next line.
#[test]
fn the_check_output_modal_drops_trailing_whitespace_that_overflows() {
    use crate::tui::app::{Modal, Tab};
    let _home = crate::testutil::HomeSandbox::new();
    let body = "error: the config file failed to parse ok   \nnext line";
    let mut app = empty_app(Tab::Services);
    app.open_modal(Modal::CheckOutput(body.to_string()));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
    term.draw(|fr| crate::tui::render::draw(fr, &app)).unwrap();
    let got = check_output_body_rows(&term);
    // 80 cols -> 48-wide modal, 46 interior, 42 content cells. The 44-cell
    // line keeps the one trailing space that fits and drops the two that
    // straddle the row end, so no blank row sits before `next line`.
    let blank = format!("│{}│", " ".repeat(46));
    let want = vec![
        blank.clone(),
        format!("│  {}  │", "error: the config file failed to parse ok "),
        format!("│  next line{}│", " ".repeat(35)),
        blank,
    ];
    assert_eq!(
        got, want,
        "trailing whitespace past the row end draws no blank row"
    );
}

/// The help modal's wrapped shape at the common 80-column width (a 48-wide
/// modal): a long description wraps onto the next row under the key column.
/// One exact pin here, beside the widened (content-fit) help pins above.
#[test]
fn the_help_modal_wraps_long_rows_at_the_common_width() {
    use ratatui::{Terminal, backend::TestBackend};

    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = empty_app(Tab::Overview);
    let mut term = Terminal::new(TestBackend::new(80, 60)).unwrap();
    term.draw(|f| draw_help(f, f.area(), &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let sliced = slice_modal(&rows);
    let want: Vec<String> = [
        "╭─ KEYS ───────────────────────────────────────╮",
        "│                                              │",
        "│  THIS MODAL                                ┃ │",
        "│                                            ┃ │",
        "│    ↑ ↓                 scroll              ┃ │",
        "│    esc · q · ?         close               ┃ │",
        "│                                            ┃ │",
        "│  ACCOUNTS                                  ┃ │",
        "│                                            ┃ │",
        "│    ↑ ↓                 move cursor         ┃ │",
        "│    ↵                   switch to selected  ┃ │",
        "│  account (confirm)                         ┃ │",
        "│    shift ↑ ↓           reorder account up /┃ │",
        "│  down                                      ┃ │",
        "│                                            ┃ │",
        "│  GLOBAL                                    ┃ │",
        "│                                            ┃ │",
        "│    n                   new account         ┃ │",
        "│    r                   refresh usage now   ┃ │",
        "│    t                   rotate all tokens   ┃ │",
        "│    d                   resolve credential  ┃ │",
        "│  divergence (when flagged)                 ┃ │",
        "│    ?                   toggle this help    ┃ │",
        "│    a                   actions             ┃ │",
        "│    x                   dismiss toast /     ┃ │",
        "│  alert                                     ┃ │",
        "│    q                   back / quit         ┃ │",
        "│    ← →                 switch screen       ┃ │",
        "│    tab                 next screen         ┃ │",
        "│    ⇧tab                previous screen     ┃ │",
        "│    ⌥1-9                jump to tab         ┃ │",
        "│    esc                 back within a       ┃ │",
        "│  sub-view (no-op at the top level)         ┃ │",
        "│    pgup/pgdn           scroll a page       ┃ │",
        "│    ⌃c                  quit from anywhere  ┃ │",
        "│                                            ┃ │",
        "│  GLYPHS                                    ┃ │",
        "│                                            ┃ │",
        "│    ●                   the active account  ┃ │",
        "│    ⇄                   a live session here ┃ │",
        "│  follows the fallback chain                ┃ │",
        "│    ↲                   the chain switches  ┃ │",
        "│  to this account next                      ┃ │",
        "│    ⊖                   disabled            ┃ │",
        "│    ⊖                   canceled            ┃ │",
        "│    ×                   auth broken         ┃ │",
        "│    ×                   key rejected        ┃ │",
        "│    ⊘                   weekly spent        ┃ │",
        "│    ⧗                   claude code blocked ┃ │",
        "│    $                   extra usage spent   ┃ │",
        "│    ◔                   5h window spent     ┃ │",
        "│    ⊘                   one model's week    ┊ │",
        "│  spent, other models ok                    ┊ │",
        "│    ~                   past the weekly     ┊ │",
        "│                                              │",
        "╰──────────────────────────────────────────────╯",
    ]
    .into_iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(sliced, want, "the wrapped help modal at 80×60");
}

/// the `stop shunt?` detail at 80 columns breaks between words,
/// no row exceeds the inner width, and both buttons stay visible one cell apart.
#[test]
fn the_stop_shunt_confirm_wraps_its_detail_between_words() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let state = ConfirmState {
        message: "stop shunt?".into(),
        detail: Some(
            "2 live sessions use it; in-flight requests drain, then the gateway stays off until the daemon restarts."
                .into(),
        ),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::StopShunt,
    };
    let (_buf, rows, _origin) = render_confirm(&state, 80, 24);
    assert_eq!(
        rows[0].chars().count(),
        48,
        "the modal is 48 wide at 80 cols"
    );
    let inner_w = 42usize;
    let source = "2 live sessions use it; in-flight requests drain, then the gateway stays off until the daemon restarts.";
    let detail_rows = &rows[3..6];
    for row in detail_rows {
        assert!(
            row.trim_matches('\u{2502}').trim().chars().count() <= inner_w,
            "no detail row exceeds the inner width: {row}"
        );
    }
    let rejoined = detail_rows
        .iter()
        .map(|r| r.trim_matches('\u{2502}').trim())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        rejoined, source,
        "the detail breaks between words, losing nothing"
    );
    assert_eq!(
        button_tail(&rows),
        "cancel   stop",
        "both buttons, one cell apart"
    );
}

/// a modal text field (the shared name prompt) keeps the caret on its
/// input cell while a typed value overruns the window, at 80 and 45 columns,
/// frame to frame. A regression guard: word wrap must never fold the field.
#[test]
fn the_modal_field_keeps_its_caret_on_the_input_across_typed_overflow() {
    use crate::testutil::{draw_caret, key, right_border, symbol_at};
    use crate::tui::app::{InputState, Modal, NamePromptAction, NamePromptForm, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;

    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    for w in [80u16, 45u16] {
        let mut app = empty_app(Tab::Overview);
        app.open_modal(Modal::NamePrompt(NamePromptForm {
            input: InputState::new(""),
            action: NamePromptAction::DuplicateProfile("src".into()),
        }));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, 20)).unwrap();
        for ch in "a".repeat(60).chars() {
            handle_key(&mut app, key(KeyCode::Char(ch)));
            let (caret, buf) = draw_caret(&mut term, |f| crate::tui::render::draw(f, &app));
            let border = right_border(&buf, caret.y);
            assert!(
                caret.x + 2 <= border,
                "[{w}] caret {caret:?} leaves the field before border {border}"
            );
        }
        handle_key(&mut app, key(KeyCode::Left));
        let (caret, buf) = draw_caret(&mut term, |f| crate::tui::render::draw(f, &app));
        assert!(
            symbol_at(&buf, caret.x, caret.y) != " ",
            "[{w}] the caret sits on an input cell"
        );
    }
}

/// A modal text field whose caret lands inside a drawn toast hides the native
/// cursor this frame (it would blink through the toast's glass); clear of the
/// stack, the caret shows as before.
#[test]
fn a_toast_over_the_modal_caret_hides_the_native_cursor() {
    use crate::actions::{AnchorAction, CaptureSnapshot};
    use crate::tui::app::{CaptureNameForm, InputState, Modal, ToastKind};

    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);

    let mut app = empty_app(Tab::Overview);
    app.modals.push(Modal::CaptureName(CaptureNameForm {
        snapshot: Box::new(CaptureSnapshot {
            credentials: None,
            base_url: Some("https://new.example.com".to_string()),
            api_key: Some("new-key".to_string()),
            anchor: AnchorAction::Unproven,
            plan: None,
        }),
        input: InputState::new("a name typed into the field"),
        from_divergence: false,
    }));

    // At 80x24 the caret is clear of the stack: it lands on the same cell it
    // lands on with no toasts.
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        term.backend().cursor_visible(),
        "the field caret shows with no toasts"
    );
    let baseline = term.backend().cursor_position();

    let wide = "x".repeat(53);
    for name in ["one", "two", "three"] {
        app.toast(ToastKind::Danger, format!("{name}\n{wide}\n{name} tail"));
    }

    // The capture modal's name caret sits under the toast stack on these two
    // sizes, so no native cursor is placed.
    for (w, h) in [(45u16, 14u16), (60u16, 12u16)] {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
        assert!(
            !term.backend().cursor_visible(),
            "[{w}x{h}] the caret hides under a toast"
        );
    }

    // At 80x24 the caret is clear of the stack and stays on the field cell.
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        term.backend().cursor_visible(),
        "the caret shows clear of the toast"
    );
    assert_eq!(
        term.backend().cursor_position(),
        baseline,
        "the caret stays on the field cell"
    );
}

/// a confirm whose word-wrapped body outgrows a 14-row terminal still
/// scrolls by the rendered viewport and reaches its tail, with the buttons
/// pinned; the offset persists across frames (draw, key, draw). PageDown and
/// PageUp step one `page_step(viewport)` each, so every body row appears in
/// some frame — a step that skips rows (a `viewport + 2` arm) must red here.
#[test]
fn a_word_wrapped_confirm_scrolls_to_its_tail_on_a_short_terminal() {
    use crate::testutil::key;
    use crate::tui::app::{
        App, ConfirmAction, ConfirmScroll, ConfirmState, Modal, Tab, handle_key, page_step,
    };
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;

    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = empty_app(Tab::Overview);
    let detail = (0..60)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    app.open_modal(Modal::Confirm(ConfirmState {
        message: "replace the stored credentials?".into(),
        detail: Some(format!("{detail} and the tail word is TAILMARK")),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::RotateAll,
    }));
    let mut term = ratatui::Terminal::new(TestBackend::new(80, 14)).unwrap();
    let scroll = |app: &App| -> (usize, usize, usize) {
        match app.modals.last() {
            Some(Modal::Confirm(state)) => (
                state.scroll.offset as usize,
                state.scroll.max_scroll.get() as usize,
                state.scroll.viewport.get() as usize,
            ),
            _ => panic!("confirm modal expected"),
        }
    };
    let draw = |app: &App, term: &mut ratatui::Terminal<TestBackend>| {
        term.draw(|f| crate::tui::render::draw(f, app)).unwrap();
    };
    draw(&app, &mut term);
    let before = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(!before.contains("TAILMARK"), "the tail starts offscreen");
    let (offset, max, viewport) = scroll(&app);
    assert_eq!(offset, 0, "starts at the top");
    assert!(max > 0, "the body overflows");
    let step = page_step(viewport);
    let mut seen = vec![false; max + viewport];
    let mark = |offset: usize, seen: &mut [bool]| {
        let end = (offset + viewport).min(seen.len());
        for s in seen.iter_mut().take(end).skip(offset) {
            *s = true;
        }
    };
    mark(offset, &mut seen);

    // Page down to the tail: every step is one page, nothing is skipped.
    let mut offset = offset;
    while offset < max {
        handle_key(&mut app, key(KeyCode::PageDown));
        draw(&app, &mut term);
        let (next, _, _) = scroll(&app);
        if next < max {
            assert_eq!(next - offset, step, "PageDown steps one page");
        } else {
            assert!(
                next - offset <= step,
                "the last PageDown clamps to the tail"
            );
        }
        offset = next;
        mark(offset, &mut seen);
    }
    assert!(
        crate::testutil::buffer_rows(term.backend().buffer())
            .concat()
            .contains("TAILMARK"),
        "the tail is on screen at the bottom"
    );

    // Page up to the top, symmetric.
    while offset > 0 {
        handle_key(&mut app, key(KeyCode::PageUp));
        draw(&app, &mut term);
        let (next, _, _) = scroll(&app);
        if next > 0 {
            assert_eq!(offset - next, step, "PageUp steps one page");
        }
        offset = next;
        mark(offset, &mut seen);
    }
    assert_eq!(offset, 0, "PageUp returns to the top");
    assert!(
        seen.iter().all(|&s| s),
        "every body row appears in some frame"
    );

    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        rows.iter().any(|r| r.contains("cancel")),
        "the buttons stay pinned while the wrapped body scrolls"
    );
}

/// Draw `app` at 80x24 and return the buffer.
fn frame_buffer(app: &App) -> ratatui::buffer::Buffer {
    use ratatui::{Terminal, backend::TestBackend};
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, app)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, app)).unwrap();
    term.backend().buffer().clone()
}

/// The open modal's border box `(top, bottom, left, right)`, found by its
/// ACCENT_2 top-left corner: the panels behind it draw the same corner glyph in
/// their own border colour.
fn modal_bounds(buf: &ratatui::buffer::Buffer) -> (u16, u16, u16, u16) {
    let area = buf.area;
    let orange = crate::tui::theme::accent_2_color();
    for y in 0..area.height {
        for x in 0..area.width {
            let cell = &buf[(x, y)];
            if cell.symbol() == "\u{256d}" && cell.fg == orange {
                let right = (x + 1..area.width)
                    .find(|&rx| buf[(rx, y)].symbol() == "\u{256e}")
                    .expect("the modal's top-right corner");
                let bottom = (y + 1..area.height)
                    .find(|&by| buf[(x, by)].symbol() == "\u{2570}")
                    .expect("the modal's bottom-left corner");
                return (y, bottom, x, right);
            }
        }
    }
    panic!("no open modal in the frame")
}

/// The open modal's row holding the `❯` caret: `(y, left border x, right
/// border x, caret x)`.
fn modal_caret_row(buf: &ratatui::buffer::Buffer) -> (u16, u16, u16, u16) {
    let (top, bottom, left, right) = modal_bounds(buf);
    for y in top + 1..bottom {
        for x in left + 1..right {
            if buf[(x, y)].symbol() == "\u{276f}" {
                return (y, left, right, x);
            }
        }
    }
    panic!("no caret row in the modal")
}

/// The `BG_HOVER` run on row `y` between the borders, as `(cells from the left
/// border to its first cell, cells from its last cell to the right border)`.
/// Panics when the run has a hole.
fn hover_run(buf: &ratatui::buffer::Buffer, y: u16, left: u16, right: u16) -> (u16, u16) {
    let hover = crate::tui::theme::bg_hover();
    let tinted: Vec<u16> = (left + 1..right)
        .filter(|&x| buf[(x, y)].bg == hover)
        .collect();
    let (first, last) = (
        *tinted.first().expect("a tinted cell"),
        *tinted.last().expect("a tinted cell"),
    );
    assert_eq!(
        tinted.len(),
        usize::from(last - first + 1),
        "the selected row's tint has no hole on row {y}"
    );
    (first - left, right - last)
}

/// The label after a caret at `caret`: the run of cells up to the first
/// two-space gap or the right border.
fn label_cells(buf: &ratatui::buffer::Buffer, y: u16, caret: u16, right: u16) -> Vec<u16> {
    let mut cells = Vec::new();
    let mut x = caret + 2;
    while x < right {
        let blank = buf[(x, y)].symbol() == " ";
        let next_blank = x + 1 >= right || buf[(x + 1, y)].symbol() == " ";
        if blank && next_blank {
            break;
        }
        cells.push(x);
        x += 1;
    }
    cells
}

/// Whether row `y` between the modal borders holds ASCII `needle`.
fn row_has_text(
    buf: &ratatui::buffer::Buffer,
    y: u16,
    left: u16,
    right: u16,
    needle: &str,
) -> bool {
    let cells: Vec<char> = (left + 1..right)
        .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
        .collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    cells
        .windows(needle_chars.len())
        .any(|window| window == needle_chars.as_slice())
}

/// The x range of ASCII `needle` on row `y` between the modal borders; panics
/// when the row holds it nowhere.
fn row_text(
    buf: &ratatui::buffer::Buffer,
    y: u16,
    left: u16,
    right: u16,
    needle: &str,
) -> std::ops::Range<u16> {
    let cells: Vec<char> = (left + 1..right)
        .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
        .collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    let start = cells
        .windows(needle_chars.len())
        .position(|window| window == needle_chars.as_slice())
        .unwrap_or_else(|| panic!("`{needle}` on row {y}"));
    (left + 1 + start as u16)..(left + 1 + start as u16 + needle_chars.len() as u16)
}

/// Every pick-one modal (divergence, divergence target, env collision, preset
/// picker, token picker) draws its selected option with the action menu's row
/// look: `❯` in ACCENT + bold, the label in TEXT + bold, and the `BG_HOVER`
/// tint over the same span of the row the action menu tints; an unselected
/// option's label is TEXT, not bold, on no tint.
#[test]
fn pick_one_modals_take_the_action_menu_row_look() {
    use crate::presets::Preset;
    use crate::tui::app::{
        ActionMenuAction, ActionMenuState, DivergenceForm, DivergenceTargetForm, EnvCollisionForm,
        Modal, PresetPickerForm, PresetTarget, ScrollStamp, Tab, TokenPickerForm,
    };
    use crate::tui::theme;
    use ratatui::style::Modifier;
    use std::cell::{Cell, RefCell};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);

    let mut menu = empty_app(Tab::Overview);
    menu.open_modal(Modal::ActionMenu(ActionMenuState::new(
        vec![ActionMenuAction::TogglePace],
        vec![ActionMenuAction::ToggleEstimates],
        None,
        None,
    )));
    let buf = frame_buffer(&menu);
    let (y, left, right, _) = modal_caret_row(&buf);
    let menu_run = hover_run(&buf, y, left, right);

    let preset = |name: &str| Preset {
        name: name.into(),
        base_url: None,
        models: Default::default(),
        builtin: false,
    };
    let cases: Vec<(&str, Modal)> = vec![
        (
            "divergence",
            Modal::Divergence(DivergenceForm {
                active: "x".into(),
                sibling: None,
                cursor: 0,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
            }),
        ),
        (
            "divergence target",
            Modal::DivergenceTarget(DivergenceTargetForm {
                targets: vec!["b".into()],
                cursor: 1,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
            }),
        ),
        (
            "env collision",
            Modal::EnvCollision(EnvCollisionForm {
                profile: "a".into(),
                key: "K".into(),
                reason: "set by the base url field".into(),
                existing_idx: None,
                cursor: 0,
                scroll: Cell::new(0),
                reviewing_overwrite: false,
                confirm_overwrite: false,
                max_scroll: Cell::new(0),
                max_scroll_stamp: Cell::new(ScrollStamp::default()),
                viewport: Cell::new(0),
                overwrite_needs_review: Cell::new(false),
            }),
        ),
        (
            "preset picker",
            Modal::PresetPicker(PresetPickerForm {
                target: PresetTarget::NewAccount,
                presets: vec![preset("p"), preset("q")],
                cursor: 0,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
            }),
        ),
        (
            "token picker",
            Modal::TokenPicker(TokenPickerForm {
                targets: vec![("b".into(), false)],
                cursor: 1,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
                require: false,
            }),
        ),
    ];
    for (name, modal) in cases {
        let mut app = empty_app(Tab::Overview);
        app.open_modal(modal);
        let buf = frame_buffer(&app);
        let (y, left, right, caret) = modal_caret_row(&buf);
        let caret_cell = &buf[(caret, y)];
        assert_eq!(caret_cell.fg, theme::accent_color(), "{name}: caret fg");
        assert!(
            caret_cell.modifier.contains(Modifier::BOLD),
            "{name}: the caret is bold"
        );
        let label = label_cells(&buf, y, caret, right);
        assert!(!label.is_empty(), "{name}: a label follows the caret");
        for x in label {
            let cell = &buf[(x, y)];
            assert_eq!(cell.fg, theme::text_color(), "{name}: label fg at x={x}");
            assert!(
                cell.modifier.contains(Modifier::BOLD),
                "{name}: the label is bold at x={x}"
            );
        }
        assert_eq!(
            hover_run(&buf, y, left, right),
            menu_run,
            "{name}: the selected row's tint spans what the action menu's does"
        );
    }

    // An unselected option: the preset picker's second row, `q`.
    let mut app = empty_app(Tab::Overview);
    app.open_modal(Modal::PresetPicker(PresetPickerForm {
        target: PresetTarget::NewAccount,
        presets: vec![preset("p"), preset("q")],
        cursor: 0,
        scroll: Cell::new(0),
        viewport: Cell::new(0),
        starts: RefCell::new(vec![]),
        block_ends: RefCell::new(vec![]),
        total: Cell::new(0),
    }));
    let buf = frame_buffer(&app);
    let (y, _, _, caret) = modal_caret_row(&buf);
    let below = &buf[(caret + 2, y + 1)];
    assert_eq!(
        below.symbol(),
        "q",
        "the unselected option sits one row down"
    );
    assert_eq!(below.fg, theme::text_color(), "an unselected label is TEXT");
    assert!(
        !below.modifier.contains(Modifier::BOLD),
        "an unselected label is not bold"
    );
    assert_ne!(
        below.bg,
        theme::bg_hover(),
        "an unselected option carries no tint"
    );
}

/// The `+ new account` and `+ new profile` synthetic rows keep their ACCENT
/// identity: selected they carry the caret + tint like any option row but the
/// label stays ACCENT (bold); unselected the label is ACCENT, not bold, on no
/// tint.
#[test]
fn the_new_rows_keep_accent_identity_selected_and_unselected() {
    use crate::tui::app::{
        ActionMenuAction, ActionMenuState, DivergenceTargetForm, Modal, TokenPickerForm,
    };
    use crate::tui::theme;
    use ratatui::style::Modifier;
    use std::cell::{Cell, RefCell};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);

    let mut menu = empty_app(Tab::Overview);
    menu.open_modal(Modal::ActionMenu(ActionMenuState::new(
        vec![ActionMenuAction::TogglePace],
        vec![ActionMenuAction::ToggleEstimates],
        None,
        None,
    )));
    let buf = frame_buffer(&menu);
    let (y, left, right, _) = modal_caret_row(&buf);
    let menu_run = hover_run(&buf, y, left, right);

    let cases: Vec<(&str, Modal)> = vec![
        (
            "+ new account",
            Modal::DivergenceTarget(DivergenceTargetForm {
                targets: vec!["b".into()],
                cursor: 0,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
            }),
        ),
        (
            "+ new profile",
            Modal::TokenPicker(TokenPickerForm {
                targets: vec![("b".into(), false)],
                cursor: 0,
                scroll: Cell::new(0),
                viewport: Cell::new(0),
                starts: RefCell::new(vec![]),
                block_ends: RefCell::new(vec![]),
                total: Cell::new(0),
                require: false,
            }),
        ),
    ];
    for (label, modal) in cases {
        let mut app = empty_app(Tab::Overview);
        app.open_modal(modal);
        let buf = frame_buffer(&app);
        let (y, left, right, caret) = modal_caret_row(&buf);
        let caret_cell = &buf[(caret, y)];
        assert_eq!(caret_cell.fg, theme::accent_color(), "{label}: caret fg");
        assert!(
            caret_cell.modifier.contains(Modifier::BOLD),
            "{label}: the caret is bold"
        );
        let cells = label_cells(&buf, y, caret, right);
        assert!(!cells.is_empty(), "{label}: a label follows the caret");
        for x in cells {
            let cell = &buf[(x, y)];
            assert_eq!(
                cell.fg,
                theme::accent_color(),
                "{label}: the label stays ACCENT at x={x}"
            );
            assert!(
                cell.modifier.contains(Modifier::BOLD),
                "{label}: the label is bold at x={x}"
            );
        }
        assert_eq!(
            hover_run(&buf, y, left, right),
            menu_run,
            "{label}: the selected row's tint spans what the action menu's does"
        );
    }

    // Unselected: cursor 1 leaves the `+ new` row one row above the caret.
    let mut app = empty_app(Tab::Overview);
    app.open_modal(Modal::DivergenceTarget(DivergenceTargetForm {
        targets: vec!["b".into()],
        cursor: 1,
        scroll: Cell::new(0),
        viewport: Cell::new(0),
        starts: RefCell::new(vec![]),
        block_ends: RefCell::new(vec![]),
        total: Cell::new(0),
    }));
    let buf = frame_buffer(&app);
    let (y, left, _, _) = modal_caret_row(&buf);
    let above = y - 1;
    assert_eq!(
        buf[(left + 3, above)].symbol(),
        " ",
        "the unselected row's gutter is blank"
    );
    let plus = &buf[(left + 5, above)];
    assert_eq!(plus.symbol(), "+", "the unselected row is `+ new account`");
    assert_eq!(
        plus.fg,
        theme::accent_color(),
        "the unselected `+ new account` label stays ACCENT"
    );
    assert!(
        !plus.modifier.contains(Modifier::BOLD),
        "the unselected `+ new account` label is not bold"
    );
    assert_ne!(
        plus.bg,
        theme::bg_hover(),
        "the unselected `+ new account` row carries no tint"
    );

    let mut app = empty_app(Tab::Overview);
    app.open_modal(Modal::TokenPicker(TokenPickerForm {
        targets: vec![("b".into(), false)],
        cursor: 1,
        scroll: Cell::new(0),
        viewport: Cell::new(0),
        starts: RefCell::new(vec![]),
        block_ends: RefCell::new(vec![]),
        total: Cell::new(0),
        require: false,
    }));
    let buf = frame_buffer(&app);
    let (y, left, _, _) = modal_caret_row(&buf);
    let above = y - 1;
    let plus = &buf[(left + 5, above)];
    assert_eq!(
        plus.symbol(),
        "+",
        "the unselected token-picker row is `+ new profile`"
    );
    assert_eq!(
        plus.fg,
        theme::accent_color(),
        "the unselected `+ new profile` label stays ACCENT"
    );
    assert!(
        !plus.modifier.contains(Modifier::BOLD),
        "the unselected `+ new profile` label is not bold"
    );
    assert_ne!(
        plus.bg,
        theme::bg_hover(),
        "the unselected `+ new profile` row carries no tint"
    );
}

/// A selected option's tint covers every line of its block: the `has a token`
/// and `built-in` suffixes ride the selected row's tint, the env-collision
/// detail line is tinted beside its label, and a wrapped long label tints
/// every wrapped line — each tint run matching the action menu's.
#[test]
fn pick_one_suffix_env_and_wrapped_rows_tint_their_whole_block() {
    use crate::presets::Preset;
    use crate::tui::app::{
        ActionMenuAction, ActionMenuState, DivergenceTargetForm, EnvCollisionForm, Modal,
        PresetPickerForm, PresetTarget, ScrollStamp, TokenPickerForm,
    };
    use crate::tui::theme;
    use std::cell::{Cell, RefCell};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(theme::Tier::Full);

    let mut menu = empty_app(Tab::Overview);
    menu.open_modal(Modal::ActionMenu(ActionMenuState::new(
        vec![ActionMenuAction::TogglePace],
        vec![ActionMenuAction::ToggleEstimates],
        None,
        None,
    )));
    let buf = frame_buffer(&menu);
    let (y, left, right, _) = modal_caret_row(&buf);
    let menu_run = hover_run(&buf, y, left, right);

    // `has a token` suffix: token picker cursor on the profile row.
    let mut app = empty_app(Tab::Overview);
    app.open_modal(Modal::TokenPicker(TokenPickerForm {
        targets: vec![("b".into(), true)],
        cursor: 1,
        scroll: Cell::new(0),
        viewport: Cell::new(0),
        starts: RefCell::new(vec![]),
        block_ends: RefCell::new(vec![]),
        total: Cell::new(0),
        require: false,
    }));
    let buf = frame_buffer(&app);
    let (y, left, right, _) = modal_caret_row(&buf);
    for x in row_text(&buf, y, left, right, "has a token") {
        let cell = &buf[(x, y)];
        assert_eq!(
            cell.fg,
            theme::text_dim_color(),
            "the `has a token` suffix stays TEXT_DIM at x={x}"
        );
        assert_eq!(
            cell.bg,
            theme::bg_hover(),
            "the `has a token` suffix rides the tint at x={x}"
        );
    }
    assert_eq!(
        hover_run(&buf, y, left, right),
        menu_run,
        "the suffix row's tint spans what the action menu's does"
    );

    // `built-in` suffix: preset picker cursor on the built-in row.
    let mut app = empty_app(Tab::Overview);
    app.open_modal(Modal::PresetPicker(PresetPickerForm {
        target: PresetTarget::NewAccount,
        presets: vec![Preset {
            name: "p".into(),
            base_url: None,
            models: Default::default(),
            builtin: true,
        }],
        cursor: 0,
        scroll: Cell::new(0),
        viewport: Cell::new(0),
        starts: RefCell::new(vec![]),
        block_ends: RefCell::new(vec![]),
        total: Cell::new(0),
    }));
    let buf = frame_buffer(&app);
    let (y, left, right, _) = modal_caret_row(&buf);
    for x in row_text(&buf, y, left, right, "built-in") {
        let cell = &buf[(x, y)];
        assert_eq!(
            cell.fg,
            theme::text_dim_color(),
            "the `built-in` suffix stays TEXT_DIM at x={x}"
        );
        assert_eq!(
            cell.bg,
            theme::bg_hover(),
            "the `built-in` suffix rides the tint at x={x}"
        );
    }
    assert_eq!(
        hover_run(&buf, y, left, right),
        menu_run,
        "the built-in row's tint spans what the action menu's does"
    );

    // Env-collision: both the label line and its detail line are tinted.
    let mut app = empty_app(Tab::Overview);
    app.open_modal(Modal::EnvCollision(EnvCollisionForm {
        profile: "a".into(),
        key: "K".into(),
        reason: "set by the base url field".into(),
        existing_idx: None,
        cursor: 0,
        scroll: Cell::new(0),
        reviewing_overwrite: false,
        confirm_overwrite: false,
        max_scroll: Cell::new(0),
        max_scroll_stamp: Cell::new(ScrollStamp::default()),
        viewport: Cell::new(0),
        overwrite_needs_review: Cell::new(false),
    }));
    let buf = frame_buffer(&app);
    let (y, left, right, _) = modal_caret_row(&buf);
    assert_eq!(
        hover_run(&buf, y, left, right),
        menu_run,
        "the env-collision label line tints what the action menu's does"
    );
    assert_eq!(
        hover_run(&buf, y + 1, left, right),
        menu_run,
        "the env-collision detail line is tinted too"
    );
    assert_eq!(
        buf[(left + 7, y + 1)].fg,
        theme::text_dim_color(),
        "the env-collision detail line stays TEXT_DIM"
    );

    // A wrapped long label tints every line of its block, bounded by the
    // label's own text rather than a row-count floor: the block runs from the
    // caret row through the row holding the label's closing `picker'`, and the
    // row after it is untinted.
    let mut app = empty_app(Tab::Overview);
    app.open_modal(Modal::DivergenceTarget(DivergenceTargetForm {
        targets: vec![
            "a-very-long-account-name-that-wraps-across-several-rows-in-the-picker".into(),
        ],
        cursor: 1,
        scroll: Cell::new(0),
        viewport: Cell::new(0),
        starts: RefCell::new(vec![]),
        block_ends: RefCell::new(vec![]),
        total: Cell::new(0),
    }));
    let buf = frame_buffer(&app);
    let (_, bottom, left, right) = modal_bounds(&buf);
    let (y, _, _, _) = modal_caret_row(&buf);
    let mut label_end = y;
    while label_end < bottom && !row_has_text(&buf, label_end, left, right, "picker'") {
        label_end += 1;
    }
    assert!(
        label_end < bottom,
        "the wrapped label's closing `picker'` sits below the caret row"
    );
    for yy in y..=label_end {
        assert_eq!(
            buf[(left + 3, yy)].bg,
            theme::bg_hover(),
            "the wrapped label row {yy} is tinted"
        );
        assert_eq!(
            hover_run(&buf, yy, left, right),
            menu_run,
            "the wrapped label row {yy} tints what the action menu's does"
        );
    }
    assert!(
        label_end + 1 < bottom,
        "the wrapped label has a row after its last line"
    );
    assert_ne!(
        buf[(left + 3, label_end + 1)].bg,
        theme::bg_hover(),
        "the row after the wrapped label is untinted"
    );
}

/// The acknowledgment modal's one button takes the action verb `close`,
/// default-focused as the neutral inverse block; no ` ok ` button remains.
#[test]
fn the_acknowledgment_button_reads_close() {
    use crate::tui::app::{ConfirmAction, ConfirmScroll, ConfirmState};
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let state = ConfirmState {
        message: "account in use".into(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::Acknowledge,
    };
    let (buf, rows, origin) = render_confirm(&state, 60, 12);
    assert!(
        !rows.iter().any(|r| r.contains(" ok ")),
        "no ` ok ` button remains:\n{}",
        rows.join("\n")
    );
    let (y, row) = rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.trim_matches(|c| c == '\u{2502}' || c == ' ') == "close")
        .unwrap_or_else(|| panic!("a `close` button row:\n{}", rows.join("\n")));
    let x = row
        .chars()
        .position(|c| c == 'c')
        .expect("the button label");
    let cell = modal_cell(&buf, origin, x, y);
    assert_eq!(
        cell.bg,
        crate::tui::theme::text_color(),
        "the focused `close` is the neutral inverse block"
    );
}

/// The loss warning's body is lowercase: the question and both sentences,
/// tab names included, pinned row by row.
#[test]
fn the_loss_warning_body_is_lowercase() {
    use crate::tui::app::{LossWarningDrop, LossWarningState, Modal, Tab};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = empty_app(Tab::Setup);
    app.open_modal(Modal::LossWarning(LossWarningState {
        choice: false,
        target: Tab::Usage,
        drop: LossWarningDrop::SetupDraft,
    }));
    let buf = frame_buffer(&app);
    let (top, bottom, left, right) = modal_bounds(&buf);
    let body: Vec<String> = (top + 1..bottom)
        .map(|y| {
            (left + 1..right)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .filter(|r| !r.is_empty())
        .take(3)
        .collect();
    assert_eq!(
        body,
        [
            "discard the unsaved edit?",
            "there are unsaved edits on the setup tab.",
            "fallback also edits this account.",
        ],
        "the loss warning body rows"
    );
}

/// No help-modal row, on any tab or descended pane, spells the page keys as
/// `page up` / `page down`: they read `pgup/pgdn`, joined.
#[test]
fn the_help_modal_writes_page_keys_joined() {
    use crate::tui::app::Modal;
    let _home = crate::testutil::HomeSandbox::new();
    let mut joined = false;
    for tab in Tab::ALL {
        for descended in [false, true] {
            let mut app = empty_app(tab);
            if descended {
                app.config_focus = ConfigFocus::Actions;
                app.fallback_focus = FallbackFocus::Detail;
                app.status.focus = StatusFocus::Detail;
                app.services.focus = ServicesFocus::Detail;
                app.token_view = TokenView::Models;
            }
            app.open_modal(Modal::Help);
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 200)).unwrap();
            term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
            let rows = crate::testutil::buffer_rows(term.backend().buffer());
            for row in &rows {
                assert!(
                    !row.contains("page up") && !row.contains("page down"),
                    "{tab:?} (descended {descended}) help spells a page key out: {row:?}"
                );
            }
            joined |= rows.iter().any(|r| r.contains("pgup/pgdn"));
        }
    }
    assert!(joined, "some help section names `pgup/pgdn`");
}
