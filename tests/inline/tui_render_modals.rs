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
                "│    page up             scroll account detail up a viewport",
                "│    page down           scroll account detail down a viewport",
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
                "│    page up             scroll model detail up a viewport",
                "│    page down           scroll model detail down a viewport",
                "│    esc                 return to model selector",
                "│    ←                   return to model selector",
                "│    c                   count cache in token figures",
                "│    t                   cycle period · lifetime / daily / weekly / monthly",
                "│    r                   reload on-disk stats",
            ],
        ),
    ];
    for (name, app, section, expected) in cases {
        let mut term = Terminal::new(TestBackend::new(100, 60)).unwrap();
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
                "│    page up             scroll account detail up a viewport",
                "│    page down           scroll account detail down a viewport",
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
                "│    page up             scroll model detail up a viewport",
                "│    page down           scroll model detail down a viewport",
                "│    esc                 return to model selector",
                "│    ←                   return to model selector",
                "│    c                   count cache in token figures",
                "│    t                   cycle period · lifetime / daily / weekly / monthly",
                "│    r                   reload on-disk stats",
            ],
        ),
    ];
    for (name, app, screen) in cases {
        let mut term = Terminal::new(TestBackend::new(100, 60)).unwrap();
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
            "│    tab                 next tab",
            "│    shift tab           previous tab",
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
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();

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
    let mut term = Terminal::new(TestBackend::new(100, 60)).unwrap();
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
    let tabs = rows
        .iter()
        .position(|r| r.contains("TABS"))
        .expect("the tabs section");
    assert!(tabs < head, "it follows the tabs section");

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
    let mut term = Terminal::new(TestBackend::new(100, 60)).unwrap();
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
        rows[head..head + 17].iter().map(slice).collect::<Vec<_>>(),
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
            "│    ⋯                   no usage yet                                     │"
                .to_string(),
        ],
    );

    // Every mark's own hue, read off the rendered cell. `⊖` and `⊘` are the
    // whole point: same shape, different color, different meaning. `×` and `⋯`
    // keep one color for both their meanings.
    let expected: [Color; 15] = [
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
        crate::tui::theme::text_faint_color(),
    ];
    // `left + 5`: the modal border, its 2-cell padding, and the row's own
    // 2-space gutter all sit ahead of the mark.
    let glyph_x = left + 5;
    let stride = buf.area.width as usize;
    let got: Vec<Color> = (0..15)
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
    app.profile_cursor = 0;
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
    use crate::tui::app::{ConfirmAction, ConfirmState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let state = ConfirmState {
        message: "mixing api-key and oauth accounts can leave sessions stuck on the \
                  api account."
            .into(),
        detail: Some("api → oauth switches may not work until cc restarts.".into()),
        choice: false,
        on_confirm: ConfirmAction::AddChainCandidate("test_name".into()),
    };
    let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
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

/// The shunt confirm buttons name their own verb (adopt / move / add), never
/// the generic `confirm` — the `AddChainCandidate` named-button extension.
#[test]
fn the_shunt_confirm_buttons_name_their_verb() {
    use crate::tui::app::{ConfirmAction, ConfirmState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let label = |action: ConfirmAction| -> String {
        let state = ConfirmState {
            message: "m".into(),
            detail: None,
            choice: false,
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
    use crate::tui::app::{ConfirmAction, ConfirmState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let render = |action: ConfirmAction| -> (String, ratatui::buffer::Buffer) {
        let state = ConfirmState {
            message: "m".into(),
            detail: None,
            choice: false,
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
    use crate::tui::app::{ConfirmAction, ConfirmState};
    use crate::tui::theme;
    use ratatui::style::Modifier;

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let state = ConfirmState {
        message: "start daemon?".into(),
        detail: None,
        choice: false,
        on_confirm: ConfirmAction::StartDaemon,
    };
    let (buf, rows, origin) = render_confirm(&state, 80, 24);
    assert_eq!(
        rows,
        vec![
            "╭─ CONFIRM ──────────────────────────────────────────╮".to_string(),
            "│                                                    │".to_string(),
            "│  start daemon?                                     │".to_string(),
            "│  spawns the clauth daemon detached: clauth daemon  │".to_string(),
            "│  logfile at ~/.clauth/daemon.log.                  │".to_string(),
            "│                                                    │".to_string(),
            "│                                 cancel     start   │".to_string(),
            "│                                                    │".to_string(),
            "╰────────────────────────────────────────────────────╯".to_string(),
        ],
    );
    let fg = |x: usize, y: usize| modal_cell(&buf, origin, x, y).fg;
    // Content starts at column 3: the border plus two cells of padding.
    assert_eq!(fg(3, 2), theme::text_color(), "the message is TEXT");
    for x in 3..38 {
        assert_eq!(
            fg(x, 3),
            theme::text_dim_color(),
            "the prefix is TEXT_DIM at {x}"
        );
    }
    for x in 38..51 {
        let cell = modal_cell(&buf, origin, x, 3);
        assert_eq!(
            cell.fg,
            theme::accent_color(),
            "the command is ACCENT at {x}"
        );
        assert!(!cell.modifier.contains(Modifier::BOLD), "never bold at {x}");
    }
    for x in 3..35 {
        assert_eq!(
            fg(x, 4),
            theme::text_dim_color(),
            "the logfile line is TEXT_DIM at {x}"
        );
    }
    // ` cancel ` spans columns 33..41, ` start ` 44..51.
    for x in 33..41 {
        let cell = modal_cell(&buf, origin, x, 6);
        assert_eq!(
            (cell.fg, cell.bg),
            (theme::bg(), theme::text_color()),
            "cancel is the focused inverse block at {x}"
        );
    }
    for x in 44..51 {
        assert_eq!(fg(x, 6), theme::text_dim_color(), "start is neutral at {x}");
    }
}

/// `stop shunt?` with its count clause: the exact rows and a DANGER `stop`
/// beside the focused `cancel`.
#[test]
fn the_stop_shunt_confirm_renders_its_rows_and_a_danger_stop() {
    use crate::tui::app::{ConfirmAction, ConfirmState};

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let state = ConfirmState {
        message: "stop shunt?".into(),
        detail: Some(
            "2 live sessions use it; in-flight requests drain, then the gateway stays off until the daemon restarts."
                .into(),
        ),
        choice: false,
        on_confirm: ConfirmAction::StopShunt,
    };
    // 120 columns hold the 103-cell detail on one row.
    let (buf, rows, origin) = render_confirm(&state, 120, 24);
    assert_eq!(
        rows,
        vec![
            "╭─ CONFIRM ─────────────────────────────────────────────────────────────────────────────────────────────────╮".to_string(),
            "│                                                                                                           │".to_string(),
            "│  stop shunt?                                                                                              │".to_string(),
            "│  2 live sessions use it; in-flight requests drain, then the gateway stays off until the daemon restarts.  │".to_string(),
            "│                                                                                                           │".to_string(),
            "│                                                                                         cancel     stop   │".to_string(),
            "│                                                                                                           │".to_string(),
            "╰───────────────────────────────────────────────────────────────────────────────────────────────────────────╯".to_string(),
        ],
    );
    // ` stop ` spans the last six inner columns of the button row, 100..106.
    for x in 100..106 {
        assert_eq!(
            modal_cell(&buf, origin, x, 5).fg,
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
        cursor: 23,
        scroll: std::cell::Cell::new(0),
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
        cursor: 0,
        scroll: std::cell::Cell::new(0),
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
        cursor: 0,
        scroll: std::cell::Cell::new(0),
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
        .draw(|f| super::draw(f, f.area(), &app, confirm))
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
        .draw(|f| super::draw(f, f.area(), &app, confirm))
        .unwrap();
    for _ in 0..20 {
        press(&mut app, ratatui::crossterm::event::KeyCode::Down);
    }
    let confirm = app.modals.last().unwrap();
    narrow
        .draw(|f| super::draw(f, f.area(), &app, confirm))
        .unwrap();
    let rows = crate::testutil::buffer_rows(narrow.backend().buffer());
    assert!(
        rows.iter().any(|row| row.contains("laude/sett"))
            && rows.iter().any(|row| row.contains("ings.json")),
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
        overwrite_needs_review: std::cell::Cell::new(true),
    };
    let mut wide = Terminal::new(TestBackend::new(100, 24)).unwrap();
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
        target: "work".to_string(),
        presets: Vec::new(),
        cursor: 0,
        scroll: std::cell::Cell::new(0),
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
