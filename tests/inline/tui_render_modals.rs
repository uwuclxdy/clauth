use super::*;
use crate::profile::{AppConfig, AppState};
use crate::tui::app::{
    App, ConfigFocus, FallbackFocus, PluginFocus, StatusFocus, TokenView, has_sub_focus,
};

fn empty_app(tab: Tab) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = tab;
    app
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
        app.plugin.focus = PluginFocus::Detail;
        app.token_view = TokenView::Models;

        if !has_sub_focus(&app) {
            continue;
        }

        let rows = tab_specific_rows(tab);
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
    let rows: Vec<(&str, &str)> = tab_specific_rows(tab)
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
            ("↑↓", "move cursor / detail row"),
            ("shift ↑↓", "reorder to set priority"),
            (
                "↵",
                "open · edit threshold · edit weekly at · edit max spend · toggle gates / last resort · remove · add",
            ),
            ("+ / -", "step rotate at / weekly at by 5"),
            ("↵ on rotate at", "type a value, ↵ saves"),
            ("↵ on weekly at", "type a %, empty clears"),
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
            "│    ↑↓                  scroll                                           │"
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
            ("↑↓", "pick account / + new, then a row"),
            ("↵", "open settings · edit field · flip toggle"),
            ("↵ on a field", "edit inline; ↵ again saves"),
            ("space", "cycle the model preset (model row)"),
            ("env", "+ add env · ↵ edits a value"),
            (
                "a",
                "duplicate the account · save it as a preset · apply one",
            ),
            (
                "disable / enable",
                "↵ arms disable, again confirms · enable is one press · inert while active or a live session is open",
            ),
            ("delete", "↵ once to arm, again to confirm"),
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
        rows[head..head + 14].iter().map(slice).collect::<Vec<_>>(),
        vec![
            "│  GLYPHS                                                                 │"
                .to_string(),
            "│                                                                         │"
                .to_string(),
            "│    ●                   the active account                               │"
                .to_string(),
            "│    ⇄                   a live session here follows the fallback chain   │"
                .to_string(),
            "│    ⊖                   disabled                                         │"
                .to_string(),
            "│    ⊖                   canceled                                         │"
                .to_string(),
            "│    ×                   auth broken                                      │"
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
        ],
    );

    // Every mark's own hue, read off the rendered cell. The two repeated glyphs
    // are the whole point: same shape, different color, different meaning.
    let expected: [Color; 12] = [
        crate::tui::theme::accent_2_color(),
        crate::tui::theme::text_dim_color(),
        crate::tui::theme::text_faint_color(),
        crate::tui::theme::danger_color(),
        crate::tui::theme::danger_color(),
        crate::tui::theme::danger_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::text_faint_color(),
    ];
    // `left + 5`: the modal border, its 2-cell padding, and the row's own
    // 2-space gutter all sit ahead of the mark.
    let glyph_x = left + 5;
    let stride = buf.area.width as usize;
    let got: Vec<Color> = (0..12)
        .map(|i| buf.content[(head + 2 + i) * stride + glyph_x].fg)
        .collect();
    assert_eq!(got, expected.to_vec());
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
        rows[top..top + 9].iter().map(slice).collect::<Vec<_>>(),
        vec![
            "╭ ACTIONS ─────────────── acct ╮".to_string(),
            "│                              │".to_string(),
            "│  ❯ refresh usage          r  │".to_string(),
            "│    rotate access token    t  │".to_string(),
            "│    disable account        d  │".to_string(),
            "│  ──────────────────────────  │".to_string(),
            "│    refresh all accounts   f  │".to_string(),
            "│    new account            n  │".to_string(),
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
        rows[top..top + 6].iter().map(slice).collect::<Vec<_>>(),
        vec![
            "╭ ACTIONS ─────────────────────╮".to_string(),
            "│                              │".to_string(),
            "│  ❯ refresh all accounts   f  │".to_string(),
            "│    new account            n  │".to_string(),
            "│                              │".to_string(),
            "╰──────────────────────────────╯".to_string(),
        ],
    );
}

/// A menu that is scoped end to end (the Setup tab, whose three actions all
/// work on the account being configured) still names that account, and still
/// draws no rule — there is no second group to hold off.
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
            "╭ ACTIONS ──────────── acct ╮".to_string(),
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
