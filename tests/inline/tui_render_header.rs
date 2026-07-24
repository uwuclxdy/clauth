use super::*;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::tui::app::{App, Tab};
use crate::usage::{UsageInfo, UsageWindow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::collections::BTreeMap;

fn oauth_profile(name: &str, five_hour_pct: f64) -> Profile {
    Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        session_feed: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
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
    }
}

fn provider_profile(name: &str) -> Profile {
    Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some("key".to_string()),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        session_feed: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
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
    assert_eq!(fit.bar_cells, 6, "bar must shrink first");
    assert!(fit.visible);
}

#[test]
fn gauge_fit_drops_bar_entirely_before_touching_name() {
    let fit = gauge_fit(19, 8, true);
    assert_eq!(fit.name_w, 8, "name must not shrink after bar drops");
    assert_eq!(fit.bar_cells, 0, "bar must drop before name trims");
    assert!(fit.visible);
}

#[test]
fn gauge_fit_truncates_name_only_after_bar_is_already_gone() {
    let fit = gauge_fit(13, 8, true);
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
    assert_eq!(tight.name_w, 6);

    let dash_only = gauge_fit(1, 10, false);
    assert_eq!(dash_only.name_w, 0);
    assert!(
        dash_only.visible,
        "the dash alone still renders at its 1-cell floor"
    );
}

// ── `header_height` ─────────────────────────────────────────────────────

#[test]
fn header_height_is_always_three() {
    let with_active = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    assert_eq!(header_height(&with_active), 3);

    let no_active = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);
    assert_eq!(header_height(&no_active), 3);

    let mut compact = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    compact.compact = true;
    assert_eq!(header_height(&compact), 3);
}

// ── Gauge on row 1, after account count ─────────────────────────────────

#[test]
fn gauge_after_account_count_on_wide_terminal() {
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    let row1 = row_content(&app, 120, 1);
    assert!(
        row1.starts_with("1 account"),
        "row 1 starts with account count"
    );
    assert!(row1.contains("·"), "middot separates count and gauge");
    assert!(row1.contains("uwuclxdy"), "gauge name on row 1 after count");
    assert!(row1.contains("42%"), "gauge percent on row 1");
}

#[test]
fn gauge_after_account_count_shows_bar_when_roomy() {
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    let row1 = row_content(&app, 120, 1);
    assert!(row1.contains('█'), "gauge bar visible on row 1 at 120 wide");
}

#[test]
fn gauge_dash_for_provider_after_account_count() {
    let mut app = app_with(vec![provider_profile("z.ai")], Some("z.ai"));
    app.tab = Tab::Tokens;
    let row1 = row_content(&app, 90, 1);
    assert!(row1.starts_with("1 account"), "row 1 starts with count");
    assert!(row1.contains("·"), "middot present for provider profile");
    assert!(row1.contains("z.ai"));
    assert!(row1.contains('—'), "provider shows dash, not bar/percent");
    assert!(!row1.contains('█'), "provider must not render a bar");
}

#[test]
fn gauge_hidden_in_compact_mode() {
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.compact = true;
    let rows = render_header_rows(&app, 90);
    assert_eq!(rows.len(), 3);
    assert!(
        !rows.iter().any(|r| r.contains("uwuclxdy")),
        "gauge must not render in compact mode"
    );
    assert!(rows[1].contains("1 account"));
}

#[test]
fn gauge_hidden_when_no_active_profile() {
    let app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);
    let rows = render_header_rows(&app, 90);
    assert_eq!(rows.len(), 3);
    assert!(!rows.iter().any(|r| r.contains("uwuclxdy")));
    assert!(rows[1].contains("1 account"));
}

#[test]
fn gauge_collapses_to_name_only_on_narrow_terminal() {
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    let row1 = row_content(&app, 60, 1);
    assert!(
        row1.contains("uwuclxdy"),
        "gauge name still visible at 60 wide"
    );
    assert!(row1.contains("·"), "middot present");
    assert!(row1.contains("1 account"), "account count always visible");
}

#[test]
fn gauge_on_row1_with_status_dot() {
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    let row1 = row_content(&app, 100, 1);
    assert!(row1.contains("●"), "status dot still visible");
    assert!(row1.contains("status.claude.ai"), "status label visible");
    assert!(row1.contains("1 account ·"), "count middot gauge");
}

#[test]
fn row2_is_tabs_only() {
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    let row2 = row_content(&app, 90, 2);
    assert!(row2.contains("overview"), "tabs on row 2");
    assert!(
        !row2.contains("uwuclxdy"),
        "gauge not on row 2 (it's on row 1)"
    );
}

// ── `● daemon` header dot (presence + health → color/hidden) ──────────────────

#[test]
fn daemon_dot_maps_health_to_color_and_hides_when_absent() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    use crate::daemon::DaemonHealth;
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));

    // Absent → no dot, and row 0 omits the label entirely.
    app.daemon_health = DaemonHealth::Absent;
    assert!(super::daemon_dot_color(&app).is_none(), "absent → hidden");
    assert!(
        !row_content(&app, 100, 0).contains("daemon"),
        "absent → row 0 omits the daemon label"
    );

    // Fresh → green, and the label appears on row 0.
    app.daemon_health = DaemonHealth::Fresh;
    assert_eq!(
        super::daemon_dot_color(&app),
        Some(super::theme::success_color()),
        "fresh → green"
    );
    let row0 = row_content(&app, 100, 0);
    assert!(
        row0.contains("● daemon"),
        "present → row 0 shows `● daemon`"
    );

    // Stale → amber.
    app.daemon_health = DaemonHealth::Stale;
    assert_eq!(
        super::daemon_dot_color(&app),
        Some(super::theme::warning_color()),
        "stale → amber"
    );
}
