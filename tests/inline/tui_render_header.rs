use super::*;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::tui::app::{App, Tab};
use crate::usage::{UsageInfo, UsageWindow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::collections::BTreeMap;

fn oauth_profile(name: &str, five_hour_pct: f64) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
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
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: five_hour_pct,
                resets_at: None,
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
        provider: None,
        third_party_usage: None,
        usage_stale: false,
    }
}

fn provider_profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some("key".to_string()),
        auto_start: false,
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
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
        usage_stale: false,
    }
}

fn app_with(profiles: Vec<Profile>, active: Option<&str>) -> App {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let config = AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names,
            ..AppState::default()
        },
        profiles,
    };
    App::new(config)
}

/// Renders only the header block (sized by `header_height`).
fn render_header_rows(app: &App, width: u16) -> Vec<String> {
    let height = header_height(app);
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, app);
    })
    .unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
}

/// A header row's content past the claude-glyph column (0..10).
fn row_content(app: &App, width: u16, row: usize) -> String {
    render_header_rows(app, width)[row]
        .chars()
        .skip(10)
        .collect()
}

// ── `gauge_fit` — collapse ladder: bar before name ──────────────────────

#[test]
fn gauge_fit_shows_full_name_bar_pct_when_roomy() {
    let fit = gauge_fit(100, 8, true);
    assert_eq!(
        fit,
        GaugeFit {
            name_w: 8,
            bar_cells: 10,
            visible: true,
        }
    );
}

#[test]
fn gauge_fit_shrinks_bar_first_before_name() {
    let fit = gauge_fit(23, 8, true);
    assert_eq!(fit.name_w, 8, "name must stay full while bar still shrinks");
    assert_eq!(fit.bar_cells, 7, "bar must shrink first");
    assert!(fit.visible);
}

#[test]
fn gauge_fit_drops_bar_entirely_before_touching_name() {
    let fit = gauge_fit(18, 8, true);
    assert_eq!(fit.name_w, 8, "name must not shrink after bar drops");
    assert_eq!(fit.bar_cells, 0, "bar must drop before name trims");
    assert!(fit.visible);
}

#[test]
fn gauge_fit_truncates_name_only_after_bar_is_already_gone() {
    let fit = gauge_fit(12, 8, true);
    assert_eq!(fit.bar_cells, 0);
    assert_eq!(fit.name_w, 7);
    assert!(fit.visible);
}

#[test]
fn gauge_fit_drops_name_only_after_bar_is_already_gone() {
    let fit = gauge_fit(7, 8, true);
    assert_eq!(fit.bar_cells, 0);
    assert_eq!(fit.name_w, 0);
    assert!(fit.visible, "the percent figure alone should still render");
}

#[test]
fn gauge_fit_hides_entirely_below_the_percent_width() {
    let fit = gauge_fit(3, 8, true);
    assert_eq!(fit, GaugeFit::HIDDEN);
}

#[test]
fn gauge_fit_provider_profile_never_shows_a_bar() {
    let roomy = gauge_fit(100, 10, false);
    assert_eq!(
        roomy,
        GaugeFit {
            name_w: 10,
            bar_cells: 0,
            visible: true
        }
    );

    let tight = gauge_fit(9, 10, false);
    assert_eq!(tight.bar_cells, 0);
    assert_eq!(tight.name_w, 8);

    let dash_only = gauge_fit(1, 10, false);
    assert_eq!(dash_only.name_w, 0);
    assert!(
        !dash_only.visible,
        "no bar and no tail means nothing to render"
    );
}

// ── `header_height` ─────────────────────────────────────────────────────

#[test]
fn header_height_is_always_three() {
    let _home = crate::testutil::HomeSandbox::new();
    let with_active = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    assert_eq!(header_height(&with_active), 3);

    let no_active = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);
    assert_eq!(header_height(&no_active), 3);

    let mut compact = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    compact.compact = true;
    assert_eq!(header_height(&compact), 3);
}

// ── Row 1: the live count, the gauge and the status indicator ────────────
//
// Row 1's text column starts after the 10-cell glyph column, so a terminal `W`
// columns wide offers it `W - 10`. The indicator `● status.claude.ai` is 18
// cells (dot, space, 16-char feed) plus a 3-cell reserve, so the gauge is
// fitted to `W - 31` and the indicator is gated on the gauge as rendered. No
// account count and no harness filter name takes part: both live on the
// accounts panel's title row. These fixtures run no session, so the live
// count reads `[ 0 live ]` wherever the width leaves it room.

#[test]
fn row1_is_the_live_count_the_gauge_and_the_status_indicator_when_wide() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;

    // At 120 the text column is 110 cells: `[ 0 live ] · ` is 13, the gauge's
    // widest rung renders 25 (8 name + ` ` + `42%` + ` ` + 12 bracketed bar),
    // the indicator 18, and the 54 cells between them are the elastic gap. The
    // fit budgets the pct at 4 (`100%`), so at a two-digit figure one spare
    // cell falls to the gap.
    let chars: Vec<char> = row_content(&app, 120, 1).chars().collect();
    let left: String = chars[..38].iter().collect();
    let gap: String = chars[38..92].iter().collect();
    let dot: String = chars[92..].iter().collect();
    assert_eq!(
        left, "[ 0 live ] · uwuclxdy 42% [████░░░░░░]",
        "the live count, then the gauge, lead row 1"
    );
    assert!(
        gap.chars().all(|c| c == ' '),
        "the elastic gap carries whitespace alone: {gap:?}"
    );
    assert_eq!(dot, "● status.claude.ai", "the indicator closes the row");
}

/// The ladder's first rung on the buffer: the bar gives a cell before the name
/// is touched. The bar at its widest (10 cells) puts the gauge at 25 and needs
/// a `W - 31 >= 26` budget, so 57 is the first width that holds it.
#[test]
fn row1_gauge_shrinks_its_bar_before_it_touches_the_name() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;

    assert_eq!(
        row_content(&app, 57, 1).trim_end(),
        "uwuclxdy 42% [████░░░░░░]    ● status.claude.ai",
        "at its own fit width the bar is full"
    );
    assert_eq!(
        row_content(&app, 56, 1).trim_end(),
        "uwuclxdy 42% [████░░░░░]    ● status.claude.ai",
        "one column narrower the bar gives a cell and the name stays whole"
    );
}

/// A provider profile has no usage window, so its gauge is the name and its
/// 1-cell trailing gap alone: no bar, no percent, no dash standing in for
/// either. The trailing gap merges into the row's elastic gap, so no string
/// pin can discriminate its width — the name text is the observable pin.
#[test]
fn row1_gauge_for_a_provider_profile_carries_no_bar() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![provider_profile("z.ai")], Some("z.ai"));
    app.tab = Tab::Tokens;

    // 90 - 10 = 80 text cells; the count is 13 of them, the gauge 5 (`z.ai`
    // plus its 1-cell trailing gap), the indicator 18.
    let row = row_content(&app, 90, 1);
    let left: String = row.chars().take(17).collect();
    assert_eq!(
        left, "[ 0 live ] · z.ai",
        "the live count leads, then the gauge: the name alone"
    );
    let rest: String = row.chars().skip(17).collect();
    let (gap, dot) = rest.split_at(80 - 17 - 18);
    assert!(
        gap.chars().all(|c| c == ' '),
        "the elastic gap carries whitespace alone: {gap:?}"
    );
    assert_eq!(dot, "● status.claude.ai", "the indicator closes the row");
    assert!(
        !row.contains('—'),
        "provider shows no dash when usage is absent"
    );
    assert!(!row.contains('█'), "provider must not render a bar");
    assert!(!row.contains('%'), "provider must not render a percent");
}

/// Two ways the active profile cannot be shown — compact mode, and a config
/// with no active slot — leave row 1 to the live count and the indicator.
/// Both are pinned on the tab where the gauge otherwise renders, so neither
/// passes by the Overview tab's own gauge-off rule.
#[test]
fn row1_carries_no_gauge_in_compact_mode_or_without_an_active_profile() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut compact = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    compact.compact = true;
    compact.tab = Tab::Tokens;
    let mut no_active = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);
    no_active.tab = Tab::Tokens;

    // (90 - 10) - 10 - 18 = 52: the count alone on the left, the indicator
    // right-aligned.
    let expected = format!("[ 0 live ]{}● status.claude.ai", " ".repeat(52));
    for (case, app) in [("compact", &compact), ("no active profile", &no_active)] {
        let rows = render_header_rows(app, 90);
        assert_eq!(rows.len(), 3);
        assert!(
            !rows.iter().any(|r| r.contains("uwuclxdy")),
            "{case}: the gauge is on no header row"
        );
        assert_eq!(
            row_content(app, 90, 1),
            expected,
            "{case}: row 1 is the count and the indicator alone"
        );
    }
}

/// The ladder's tail on the buffer: the bar is already gone, the name holds
/// while a `W - 31 >= 13` budget remains (13 = 8 name + ` ` + the percent's
/// 4 budgeted cells), and one column under it the name clips rather than the
/// percent.
#[test]
fn row1_gauge_falls_to_the_name_and_percent_before_the_name_clips() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;

    assert_eq!(
        row_content(&app, 44, 1).trim_end(),
        "uwuclxdy 42%    ● status.claude.ai",
        "44 is the first width whose budget holds the whole name"
    );
    assert_eq!(
        row_content(&app, 43, 1).trim_end(),
        "uwuclx… 42%    ● status.claude.ai",
        "one column narrower the name carries its truncation ellipsis"
    );
}

/// The gauge's last rung is the percent alone, and the indicator is charged
/// the gauge as rendered: it holds the row at that rung and one column under
/// it, when the gauge has gone.
#[test]
fn row1_gauge_falls_to_the_percent_alone_and_then_away() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;

    assert_eq!(
        row_content(&app, 35, 1).trim_end(),
        "42%    ● status.claude.ai",
        "the percent holds a `W - 31 >= 4` budget down to 35"
    );
    assert_eq!(
        row_content(&app, 34, 1).trim_end(),
        "      ● status.claude.ai",
        "one column narrower the gauge is gone, the indicator staying"
    );
}

// ── Row 1: the fleet's live-session count ahead of the gauge ────────────────
//
// `[ 3 live ] · ` leads row 1: a counter chip holding the count of every live
// session across the fleet, not the active account's. It takes only the width
// the gauge and the indicator leave, so it is the first thing row 1 sheds: it
// renders only while gauge + chip + indicator + the 3-cell reserve all fit.

/// Three sessions, two on the active account and one on `kerry`, an account
/// no longer in the config: the prefix counts all three, so a count taken over
/// the configured accounts alone would read 2.
fn app_with_three_live(tab: Tab) -> App {
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = tab;
    app.live_sessions = crate::live_sessions::LiveTally::of([
        crate::testutil::live_row("4242-0", "uwuclxdy"),
        crate::live_sessions::LiveSession {
            follows_chain: false,
            ..crate::testutil::live_row("4242-1", "uwuclxdy")
        },
        crate::testutil::live_row("4343-0", "kerry"),
    ]);
    app
}

/// Compact mode hides the gauge, not the fleet's count: the count stands alone
/// the way it does on the Overview.
#[test]
fn row1_keeps_the_live_count_in_compact_mode() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_three_live(Tab::Tokens);
    app.compact = true;

    assert_eq!(
        row_content(&app, 90, 1),
        format!("[ 3 live ]{}● status.claude.ai", " ".repeat(52)),
    );
}

#[test]
fn row1_leads_with_the_fleet_live_count_before_the_gauge() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_three_live(Tab::Tokens);

    // 120 - 10 = 110 text cells: `[ 3 live ] · ` is 13, the gauge 25, the
    // indicator 18, and the 54 cells between them are the elastic gap.
    assert_eq!(
        row_content(&app, 120, 1),
        format!(
            "[ 3 live ] · uwuclxdy 42% [████░░░░░░]{}● status.claude.ai",
            " ".repeat(54)
        ),
    );
}

#[test]
fn row1_carries_the_live_count_alone_where_no_gauge_renders() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_three_live(Tab::Overview);

    // No gauge on the Overview: the count stands alone, with no separator
    // left dangling after it. (90 - 10) - 10 - 18 = 52 cells of gap.
    assert_eq!(
        row_content(&app, 90, 1),
        format!("[ 3 live ]{}● status.claude.ai", " ".repeat(52)),
    );
}

/// Zero shows (cloudy, 2026-09-29): a count that vanished at zero would move
/// the gauge 13 cells each time the first session starts or the last one ends.
/// Swept over every width and three row shapes (gauge with a bar, a provider's
/// name-only gauge, no gauge), so a count-conditional term anywhere in the
/// shed gate reds wherever it bites inside 24..=140 columns.
#[test]
fn row1_shows_zero_live_so_the_gauge_holds_its_place() {
    let _home = crate::testutil::HomeSandbox::new();
    type Build = fn() -> App;
    let shapes: [(&str, Build); 3] = [
        ("oauth", || {
            let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
            app.tab = Tab::Tokens;
            app
        }),
        ("provider", || {
            let mut app = app_with(vec![provider_profile("z.ai")], Some("z.ai"));
            app.tab = Tab::Tokens;
            app
        }),
        ("no gauge", || {
            let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
            app.tab = Tab::Overview;
            app
        }),
    ];
    for (shape, build) in shapes {
        let idle = build();
        let mut busy = build();
        busy.live_sessions =
            crate::live_sessions::LiveTally::of([crate::testutil::live_row("4242-0", "uwuclxdy")]);
        for width in 24..=140u16 {
            assert_eq!(
                row_content(&idle, width, 1).replacen("[ 0 live ]", "[ 1 live ]", 1),
                row_content(&busy, width, 1),
                "{shape} at {width}: the first session changes the digit alone"
            );
        }
    }
}

/// The shed seam: gauge 25 + chip and separator 13 + indicator 18 + reserve 3
/// = 59 text cells, so 69 is the narrowest width holding the chip, and at 68
/// the chip goes whole with its ` · ` while the gauge keeps its full bar.
#[test]
fn row1_sheds_the_live_count_before_the_gauge_or_the_indicator() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_three_live(Tab::Tokens);

    assert_eq!(
        row_content(&app, 69, 1),
        "[ 3 live ] · uwuclxdy 42% [████░░░░░░]   ● status.claude.ai",
        "at its own fit width the chip renders"
    );
    assert_eq!(
        row_content(&app, 68, 1),
        format!(
            "uwuclxdy 42% [████░░░░░░]{}● status.claude.ai",
            " ".repeat(15)
        ),
        "one column narrower the count drops whole, the gauge untouched"
    );
}

fn tally_of(n: usize) -> crate::live_sessions::LiveTally {
    crate::live_sessions::LiveTally::of(
        (0..n).map(|i| crate::testutil::live_row(&format!("4242-{i}"), "uwuclxdy")),
    )
}

/// The chip's cells by role: brackets and unit `TEXT_DIM` without bold, the
/// number `TEXT` + bold, the separator to the gauge `TEXT_DIM`; at zero the
/// number drops to `TEXT_DIM` without bold. Pinned per cell off the theme's
/// own colors.
#[test]
fn the_live_chip_cells_carry_the_counter_chip_grammar() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dim = super::theme::text_dim_color();
    let text = super::theme::text_color();
    for (n, number_fg, number_bold) in [(0, dim, false), (3, text, true)] {
        let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
        app.tab = Tab::Tokens;
        app.set_live_sessions(tally_of(n));
        let height = header_height(&app);
        let mut term = Terminal::new(TestBackend::new(120, height)).unwrap();
        term.draw(|f| {
            let area = f.area();
            super::draw(f, area, &app);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let cell = |col: usize| &buf.content[120 + 10 + col];
        let text_of: String = (0..13).map(|c| cell(c).symbol().to_string()).collect();
        assert_eq!(text_of, format!("[ {n} live ] · "), "{n}: the chip's text");
        for (col, role) in [
            (0, "open bracket"),
            (4, "unit"),
            (7, "unit"),
            (9, "close bracket"),
            (11, "separator"),
        ] {
            assert_eq!(cell(col).fg, dim, "{n}: the {role} renders TEXT_DIM");
            assert!(
                !cell(col).modifier.contains(Modifier::BOLD),
                "{n}: the {role} carries no bold"
            );
        }
        assert_eq!(cell(2).fg, number_fg, "{n}: the number's color");
        assert_eq!(
            cell(2).modifier.contains(Modifier::BOLD),
            number_bold,
            "{n}: the number's weight"
        );
    }
}

/// The width hold: once the chip has read `10`, a drop to `9` keeps the
/// two-digit width, the freed cell standing blank after the closing bracket,
/// so the gauge's first cell stays in its column. A fresh app that never read
/// two digits renders the one-digit chip tight.
#[test]
fn the_live_chip_holds_its_widest_width_so_the_gauge_stays_put() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;

    app.set_live_sessions(tally_of(10));
    let at_ten = row_content(&app, 120, 1);
    assert!(
        at_ten.starts_with("[ 10 live ] · uwuclxdy 42% [████░░░░░░]"),
        "ten: {at_ten:?}"
    );

    app.set_live_sessions(tally_of(9));
    let at_nine = row_content(&app, 120, 1);
    assert!(
        at_nine.starts_with("[ 9 live ]  · uwuclxdy 42% [████░░░░░░]"),
        "nine after ten keeps the two-digit width: {at_nine:?}"
    );
    assert_eq!(
        at_ten.chars().position(|c| c == 'u'),
        Some(14),
        "the gauge's first cell sits after the two-digit chip and its separator"
    );
    assert_eq!(
        at_nine.chars().position(|c| c == 'u'),
        Some(14),
        "the gauge's first cell holds its column across 10 → 9"
    );

    let mut fresh = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    fresh.tab = Tab::Tokens;
    fresh.set_live_sessions(tally_of(9));
    assert!(
        row_content(&fresh, 120, 1).starts_with("[ 9 live ] · uwuclxdy"),
        "a chip that never read two digits renders tight"
    );
}

/// The hold is seeded at construction: a TUI opening onto ten live sessions
/// holds the two-digit width before its first re-tally, so a drop to nine
/// assigned straight onto the field (no setter to widen it) still pads.
#[test]
fn the_live_chip_hold_is_seeded_from_the_first_tally() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut markers = Vec::new();
    for i in 0..10 {
        let id = format!("4242-{i}");
        crate::live_sessions::register(&crate::testutil::live_row(&id, "uwuclxdy"))
            .expect("register a live row");
        markers.push(
            crate::runtime::hold_session_row_marker(&"uwuclxdy".into(), false, &id)
                .expect("hold the row's marker"),
        );
    }
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    assert!(
        row_content(&app, 120, 1).starts_with("[ 10 live ] · uwuclxdy"),
        "the construction tally reads all ten"
    );
    app.live_sessions = tally_of(9);
    assert!(
        row_content(&app, 120, 1).starts_with("[ 9 live ]  · uwuclxdy"),
        "the seeded hold keeps the two-digit width"
    );
}

/// A count wider than the hold renders whole: the hold pads, never clips.
#[test]
fn a_count_wider_than_the_hold_renders_whole() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.live_sessions = tally_of(100);
    assert!(
        row_content(&app, 120, 1).starts_with("[ 100 live ] · uwuclxdy"),
        "a three-digit count past a one-digit hold"
    );
}

/// A tally that could not read renders `—` where the number sits, dim and
/// unbolded, never a `0` the app did not read; the chip holds its place.
#[test]
fn an_unread_tally_renders_a_dash_not_a_zero() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(crate::live_sessions::LiveTally::unread());
    let height = header_height(&app);
    let mut term = Terminal::new(TestBackend::new(120, height)).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, &app);
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let row1: String = crate::testutil::buffer_rows(&buf)[1]
        .chars()
        .skip(10)
        .collect();
    assert!(
        row1.starts_with("[ — live ] · uwuclxdy 42% [████░░░░░░]"),
        "{row1:?}"
    );
    let dash = &buf.content[120 + 10 + 2];
    assert_eq!(
        dash.fg,
        super::theme::text_dim_color(),
        "the dash renders TEXT_DIM"
    );
    assert!(
        !dash.modifier.contains(Modifier::BOLD),
        "the dash carries no bold"
    );
}

/// The gauge rung's cells by role: the percent leads the bar, both in the util
/// color (80% → DANGER), the brackets hug the bar as TEXT_DIM chrome,
/// unbolded; every gap inside the rung is one cell. Pinned per cell off the
/// theme's own colors.
#[test]
fn the_gauge_rung_cells_carry_the_chrome_and_util_roles() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 80.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    let height = header_height(&app);
    let mut term = Terminal::new(TestBackend::new(120, height)).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, &app);
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let cell = |col: usize| &buf.content[120 + 10 + 13 + col];
    let text_of: String = (0..25).map(|c| cell(c).symbol().to_string()).collect();
    assert_eq!(text_of, "uwuclxdy 80% [████████░░]", "{text_of:?}");
    let dim = super::theme::text_dim_color();
    let danger = super::theme::danger_color();
    for (col, role) in [(13, "open bracket"), (24, "close bracket")] {
        assert_eq!(cell(col).fg, dim, "{role} renders TEXT_DIM");
        assert!(
            !cell(col).modifier.contains(Modifier::BOLD),
            "{role} carries no bold"
        );
    }
    for col in 9..=11 {
        assert_eq!(
            cell(col).fg,
            danger,
            "the percent renders the util color at {col}"
        );
    }
    for col in 14..=23 {
        assert_eq!(
            cell(col).fg,
            danger,
            "the bar renders the util color at {col}"
        );
    }
}

/// The held width counts toward the shed gate: after `10`, a `9` chip still
/// charges the two-digit width, so it sheds at the width the `10` chip did.
#[test]
fn the_held_live_chip_width_is_charged_to_the_shed_gate() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(tally_of(10));
    app.set_live_sessions(tally_of(9));

    // gauge 25 + held chip and separator 14 + indicator 18 + reserve 3 = 60.
    assert!(
        row_content(&app, 70, 1).starts_with("[ 9 live ]  · uwuclxdy"),
        "70 holds the held-width chip"
    );
    assert!(
        row_content(&app, 69, 1).starts_with("uwuclxdy 42%"),
        "69 sheds it whole, as it shed the `10` chip"
    );
}

#[test]
fn row2_is_tabs_only() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    let row2 = row_content(&app, 90, 2);
    assert!(row2.contains("overview"), "tabs on row 2");
    assert!(
        !row2.contains("uwuclxdy"),
        "gauge not on row 2 (it's on row 1)"
    );
}

// ── `[ daemon ]` header chip (always present; health → color) ────────────────

#[test]
fn the_daemon_chip_is_always_present_and_maps_health_to_color() {
    // `HomeSandbox` outermost: its `HOME_TEST_LOCK` must not be taken while a
    // RankedMutex (here `TierSandbox`'s) is held.
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    use crate::daemon::DaemonHealth;
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));

    // Absent → the chip still renders, faint, on the right edge.
    app.daemon_health = DaemonHealth::Absent;
    assert_eq!(
        super::daemon_chip_color(&app),
        super::theme::text_faint_color(),
        "absent → TEXT_FAINT, the service-chip contract's absent tone"
    );
    let row0 = row_content(&app, 100, 0);
    assert!(
        row0.trim_end().ends_with("[ daemon ]"),
        "absent → the chip is still on the row: {row0:?}"
    );

    // Fresh → green.
    app.daemon_health = DaemonHealth::Fresh;
    assert_eq!(
        super::daemon_chip_color(&app),
        super::theme::success_color(),
        "fresh → green"
    );

    // Stale → amber.
    app.daemon_health = DaemonHealth::Stale;
    assert_eq!(
        super::daemon_chip_color(&app),
        super::theme::warning_color(),
        "stale → amber"
    );
}

/// The chip's pill grammar on the buffer: `[ ` and ` ]` are TEXT_DIM chrome,
/// the word between them bold in the health color (faint when absent) — pinned on
/// cells, so a chip painted in one flat style reds.
#[test]
fn the_daemon_chip_cells_carry_the_pill_grammar() {
    use crate::daemon::DaemonHealth;
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    let word_w = "daemon".chars().count();

    for (health, expected) in [
        (DaemonHealth::Absent, super::theme::text_faint_color()),
        (DaemonHealth::Fresh, super::theme::success_color()),
        (DaemonHealth::Stale, super::theme::warning_color()),
    ] {
        app.daemon_health = health;
        let width = 100;
        let height = header_height(&app);
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| {
            let area = f.area();
            super::draw(f, area, &app);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row = crate::testutil::buffer_rows(buf)[0]
            .chars()
            .skip(10)
            .collect::<String>();
        let chip = 10 + row.find("[ daemon ]").expect("chip renders");

        // Cells 0-1 are `[ `, the last two ` ]`: chrome either side.
        for cell in [chip, chip + 1, chip + word_w + 2, chip + word_w + 3] {
            assert_eq!(
                buf.content[cell].fg,
                super::theme::text_dim_color(),
                "{health:?}: the bracket at cell {cell} is TEXT_DIM chrome"
            );
            assert!(
                !buf.content[cell].modifier.contains(Modifier::BOLD),
                "{health:?}: the bracket at cell {cell} is not bold"
            );
        }
        // Cells 2..word_w+2 are the word: bold, in the health color.
        for i in 0..word_w {
            let cell = chip + 2 + i;
            assert_eq!(
                buf.content[cell].fg, expected,
                "{health:?}: the word carries the health color at cell {cell}"
            );
            assert!(
                buf.content[cell].modifier.contains(Modifier::BOLD),
                "{health:?}: the word is bold at cell {cell}"
            );
        }
    }
}

// ── `[ shunt ]` header chip (always present; gateway state → color) ──────────

/// Every gateway state, once each.
const GATEWAY_STATES: [crate::daemon::gateway::GatewayState; 15] = {
    use crate::daemon::gateway::GatewayState as S;
    [
        S::Absent,
        S::Disabled,
        S::Held,
        S::NoConfig,
        S::YamlRefused,
        S::Misconfigured,
        S::BinaryMissing,
        S::Foreign,
        S::Starting,
        S::Healthy,
        S::Unhealthy,
        S::BelowFloor,
        S::Restarting,
        S::Stopping,
        S::Unobserved,
    ]
};

/// The chip's color per state, spelled from the design record: green serving,
/// amber starting, unhealthy or between runs, red refused (the gateway will not
/// run until its setup is fixed), faint when nothing should run. The match is
/// exhaustive, so a new state fails to compile here rather than inheriting a
/// color.
fn expected_shunt_color(state: crate::daemon::gateway::GatewayState) -> ratatui::style::Color {
    use crate::daemon::gateway::GatewayState as S;
    match state {
        S::Healthy => super::theme::success_color(),
        S::Starting | S::Unhealthy | S::Restarting | S::Stopping => super::theme::warning_color(),
        S::NoConfig
        | S::YamlRefused
        | S::Misconfigured
        | S::BinaryMissing
        | S::Foreign
        | S::BelowFloor => super::theme::danger_color(),
        S::Absent | S::Disabled | S::Held | S::Unobserved => super::theme::text_faint_color(),
    }
}

/// `[ shunt ]` renders for every state, the gateway absent included, with the
/// pill grammar on its cells: `[ ` and ` ]` TEXT_DIM and unbolded, the word bold
/// in the state's color.
#[test]
fn the_shunt_chip_is_always_present_and_maps_every_state_to_its_color() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    let word_w = "shunt".chars().count();
    for (i, a) in GATEWAY_STATES.iter().enumerate() {
        assert!(
            !GATEWAY_STATES[i + 1..].contains(a),
            "{a:?} is listed twice"
        );
    }

    for state in GATEWAY_STATES {
        app.gateway_state = state;
        let expected = expected_shunt_color(state);
        let width = 100;
        let mut term = Terminal::new(TestBackend::new(width, header_height(&app))).unwrap();
        term.draw(|f| {
            let area = f.area();
            super::draw(f, area, &app);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row = crate::testutil::buffer_rows(buf)[0]
            .chars()
            .skip(10)
            .collect::<String>();
        assert!(
            row.trim_end().ends_with("[ shunt ]  [ daemon ]"),
            "{state:?}: the shunt chip sits two cells left of the daemon chip: {row:?}"
        );
        let chip = 10 + row.find("[ shunt ]").expect("chip renders");
        for cell in [chip, chip + 1, chip + word_w + 2, chip + word_w + 3] {
            assert_eq!(
                buf.content[cell].fg,
                super::theme::text_dim_color(),
                "{state:?}: the bracket at cell {cell} is TEXT_DIM chrome"
            );
            assert!(
                !buf.content[cell].modifier.contains(Modifier::BOLD),
                "{state:?}: the bracket at cell {cell} is not bold"
            );
        }
        for i in 0..word_w {
            let cell = chip + 2 + i;
            assert_eq!(
                buf.content[cell].fg, expected,
                "{state:?}: the word carries the state's color at cell {cell}"
            );
            assert!(
                buf.content[cell].modifier.contains(Modifier::BOLD),
                "{state:?}: the word is bold at cell {cell}"
            );
        }
    }
}

/// The status indicator is right-aligned across an elastic gap, and it DROPS
/// whole when the row cannot hold it: a dot left to render would clip the feed
/// mid-word. With no gauge on the row, the indicator's own fit (`col >= 18 + 3`)
/// never passes inside the sub-30 band — there the logo column yields its
/// cells to the `Min(20)` text column, capping it at 20 — so the gate decides
/// at the band's edge: at 31 the text column is 21 and the indicator renders
/// with its 3-cell gap; at 30 it drops whole.
#[test]
fn the_status_indicator_drops_rather_than_clipping_when_the_row_runs_short() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);

    assert_eq!(
        row_content(&app, 31, 1),
        "   ● status.claude.ai",
        "exact fit: the indicator renders with its 3-cell gap"
    );
    assert_eq!(
        row_content(&app, 30, 1).trim_end(),
        "",
        "one column narrower it drops whole rather than clipping the feed"
    );
}

// ── The counts live on the accounts panel, never in the header ───────────
//
// The by-harness counts moved onto the accounts panel's title, which carries
// them as its title-right meta slot (`tui_render_overview.rs` pins that slot's
// own shed). These two pins are the guard against a count, an `ACCOUNTS`
// label or a harness filter name coming back to a header row.

/// No header row names the accounts at any width, tab or harness filter: the
/// sweep walks every seam the old count line had — the label, the count, each
/// gauge rung, the indicator — on both tabs the gauge differs between.
#[test]
fn no_header_row_counts_accounts_at_any_width_tab_or_filter() {
    use crate::tui::app::HarnessFilter;
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1", "cx2", "cx3"]);
    let mut app = app_with(
        vec![oauth_profile("uwuclxdy", 42.0), provider_profile("z.ai")],
        Some("uwuclxdy"),
    );
    // A live fleet puts the one sanctioned middot on row 1 (`[ 2 live ] · `), so
    // the guard below runs with it present and strips exactly that one.
    app.live_sessions = crate::live_sessions::LiveTally::of([
        crate::testutil::live_row("4242-0", "uwuclxdy"),
        crate::testutil::live_row("4242-1", "z.ai"),
    ]);

    for filter in [
        HarnessFilter::All,
        HarnessFilter::Claude,
        HarnessFilter::Codex,
    ] {
        app.harness_filter = filter;
        for tab in [Tab::Overview, Tab::Tokens] {
            app.tab = tab;
            for width in 24..=140u16 {
                for (row, content) in render_header_rows(&app, width).iter().enumerate() {
                    let lower = content.to_lowercase();
                    assert!(
                        !lower.contains("account"),
                        "{filter:?} on {tab:?} at {width}: header row {row} names the \
                         accounts: {content:?}"
                    );
                    // The row-18 wording (`52 claude · 1 codex`) and the chip
                    // form (`[ 52 · 1 ]`) carry no "account" substring: the
                    // harness words (space-led, so the feed's own
                    // `status.claude.ai` never trips them) and the middot each
                    // red them on their own, whatever the wording a count
                    // comes back in.
                    assert!(
                        !lower.contains(" claude") && !lower.contains("codex"),
                        "{filter:?} on {tab:?} at {width}: header row {row} names a \
                         harness: {content:?}"
                    );
                    assert!(
                        !content.replacen("[ 2 live ] · ", "", 1).contains('·'),
                        "{filter:?} on {tab:?} at {width}: header row {row} carries a \
                         count middot: {content:?}"
                    );
                }
            }
        }
    }
}

/// The word `ACCOUNTS` belongs to the accounts panel's title alone: a full
/// Overview frame carries it once, on the panel's top border. A second
/// `ACCOUNTS` label one row above that border, where the header used to carry
/// the account count, reds this.
#[test]
fn the_word_accounts_renders_on_the_panel_title_alone() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1"]);
    let mut app = app_with(
        vec![oauth_profile("uwuclxdy", 42.0), provider_profile("z.ai")],
        None,
    );
    app.tab = Tab::Overview;

    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());

    for (y, row) in rows.iter().take(header_height(&app) as usize).enumerate() {
        assert!(
            !row.contains("ACCOUNTS"),
            "header row {y} carries no ACCOUNTS: {row:?}"
        );
    }

    let carrying: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains("ACCOUNTS"))
        .map(|(y, _)| y)
        .collect();
    assert_eq!(
        carrying.len(),
        1,
        "the word renders exactly once on the whole screen: {carrying:?}"
    );
    assert!(
        rows[carrying[0]].starts_with("╭─ ACCOUNTS "),
        "and it is the accounts panel's title: {:?}",
        rows[carrying[0]]
    );
}
