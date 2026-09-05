use super::*;
use crate::profile::{AppConfig, AppState};
use crate::tui::app::App;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn truncate_label(label: String, max_chars: usize) -> String {
    let char_count = label.chars().count();
    if char_count <= max_chars {
        return label;
    }
    if max_chars == 0 {
        return String::new();
    }
    if let Some(suffix_start) = label.rfind('(') {
        let suffix = &label[suffix_start..];
        let body = &label[..suffix_start];
        let suffix_chars = suffix.chars().count();
        let body_budget = max_chars.saturating_sub(suffix_chars + 1);
        if body_budget > 0 {
            let trimmed_body: String = body.chars().take(body_budget).collect();
            return format!("{trimmed_body}…{suffix}");
        }
    }
    let trimmed: String = label.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{trimmed}…")
}

fn empty_app(active: Tab) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = active;
    app
}

fn render_tabs(app: &App, width: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(width, 1)).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, app);
    })
    .unwrap();
    crate::testutil::buffer_rows(term.backend().buffer()).concat()
}

/// The tokens dashboard must separate facts by alignment, never a `·` middot,
/// and must render the active span (earliest left, latest right).
#[test]
fn tokens_dashboard_uses_alignment_not_middot() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::pricing::HourTokens;
    use crate::tokens::{DayActivity, DaySummary, DayTokens, ModelTokens, TokenStats};
    let mut app = empty_app(Tab::Tokens);
    let daily: Vec<DayTokens> = (0..30)
        .map(|i| DayTokens {
            date: format!("2026-05-{:02}", i + 1),
            tokens: 1_000_000 + (i as u64 % 7) * 800_000,
        })
        .collect();
    let activity: Vec<DayActivity> = (0..30)
        .map(|i| DayActivity {
            date: format!("2026-05-{:02}", i + 1),
            messages: 200 + (i as u64 % 5) * 600,
            sessions: 3 + (i as u64 % 4),
            tool_calls: 100 + (i as u64 % 6) * 400,
        })
        .collect();
    let mut hour_counts = [0u64; 24];
    for (h, c) in hour_counts.iter_mut().enumerate() {
        *c = (h as u64 * 5) % 130;
    }
    app.token_stats = Some(TokenStats {
        models: vec![ModelTokens {
            model: "claude-opus-4-8".into(),
            input: 32_900_000,
            output: 72_500_000,
            cache_read: 4_765_000_000,
            cache_create: 543_000_000,
        }],
        daily,
        daily_models: vec![],
        activity,
        hour_counts,
        total_input: 2_735_000_000,
        total_output: 185_000_000,
        total_cache_read: 19_562_000_000,
        total_cache_create: 1_614_000_000,
        total_sessions: 1393,
        total_messages: 215_921,
        first_session_date: Some("2026-01-18T19:26:04Z".into()),
        last_computed_date: Some("2026-06-09".into()),
        topped_up_through: Some("2026-06-15".into()),
        today: Some(DaySummary {
            date: "2026-06-15".into(),
            input: 12_300_000,
            output: 1_400_000,
            cache_read: 88_000_000,
            cache_create: 5_200_000,
            messages: 342,
            hours: [0; 24],
            token_hours: [HourTokens::default(); 24],
            model_hours: vec![],
            models: vec![],
        }),
    });
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    term.draw(|f| super::super::draw(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = (0..30usize)
        .flat_map(|y| (0..100usize).map(move |x| (x, y)))
        .map(|(x, y)| buf.content[y * 100 + x].symbol().to_owned())
        .collect();
    // Skip the 3-row header which now has a middot in the gauge area.
    let body: String = text.chars().skip(3 * 100).take(27 * 100).collect();
    assert!(
        !body.contains('·'),
        "tokens dashboard must use alignment, not `·` separators"
    );
    assert!(text.contains("jan 18"), "span start missing");
    assert!(text.contains("jun 15"), "span end / freshness missing");
    assert!(text.contains("TODAY"), "today card missing");
    assert!(
        text.contains("opus 4.8"),
        "top models should show the mapped display name in full"
    );
    assert!(
        text.contains("13.7M"),
        "today should headline in+out tokens"
    );
    assert!(
        !text.contains("107M"),
        "today must not headline the cache-inflated total"
    );
}

#[test]
fn count_cache_toggle_switches_token_basis() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::pricing::HourTokens;
    use crate::tokens::{DaySummary, TokenStats};
    let mut app = empty_app(Tab::Tokens);
    app.token_stats = Some(TokenStats {
        models: vec![],
        daily: vec![],
        daily_models: vec![],
        activity: vec![],
        hour_counts: [0; 24],
        total_input: 10_000_000,
        total_output: 0,
        total_cache_read: 90_000_000,
        total_cache_create: 0,
        total_sessions: 1,
        total_messages: 1,
        first_session_date: None,
        last_computed_date: None,
        topped_up_through: None,
        today: Some(DaySummary {
            date: "2026-06-15".into(),
            input: 10_000_000,
            output: 0,
            cache_read: 90_000_000,
            cache_create: 0,
            messages: 1,
            hours: [0; 24],
            token_hours: [HourTokens::default(); 24],
            model_hours: vec![],
            models: vec![],
        }),
    });
    let render = |app: &App| -> String {
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| super::super::draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..30usize)
            .flat_map(|y| (0..100usize).map(move |x| (x, y)))
            .map(|(x, y)| buf.content[y * 100 + x].symbol().to_owned())
            .collect()
    };

    {
        app.config().state.count_cache = false;
    }
    let off = render(&app);
    assert!(off.contains("10.0M"), "off must headline in+out (10.0M)");
    assert!(!off.contains("100M"), "off must not count cache");

    {
        app.config().state.count_cache = true;
    }
    let on = render(&app);
    assert!(on.contains("100M"), "on must count cache (100M)");
}

#[test]
fn normal_form_shows_all_tabs() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = empty_app(Tab::Overview);
    let s = render_tabs(&app, 75);
    assert!(s.contains("overview"), "active tab missing");
    assert!(s.contains("usage"), "inactive tab missing");
    assert!(s.contains("tokens"), "inactive tab missing");
    assert!(s.contains("setup"), "inactive tab missing");
    assert!(s.contains("fallback"), "inactive tab missing");
    assert!(s.contains("config"), "inactive tab missing");
    assert!(s.contains("status"), "inactive tab missing");
    assert!(s.contains("plugin"), "inactive tab missing");
}

#[test]
fn normal_form_labels_untruncated_at_tight_boundary() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = empty_app(Tab::Overview);
    let s = render_tabs(&app, 71);
    assert!(
        s.contains("overview"),
        "overview must not be truncated at tight boundary"
    );
    assert!(
        s.contains("plugin"),
        "plugin must not be truncated at tight boundary"
    );
}

#[test]
fn overflow_form_shows_only_active() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = empty_app(Tab::Usage);
    let s = render_tabs(&app, 10);
    assert!(
        s.contains("usage"),
        "active label must appear in overflow form"
    );
    assert!(
        !s.contains("overview"),
        "non-active tab must not appear in overflow"
    );
    assert!(
        !s.contains("config"),
        "non-active tab must not appear in overflow"
    );
}

#[test]
fn overflow_chevrons_at_middle_tab() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = empty_app(Tab::Usage);
    let s = render_tabs(&app, 15);
    assert!(
        s.contains('‹'),
        "left chevron must appear for non-first tab"
    );
    assert!(
        s.contains('›'),
        "right chevron must appear for non-last tab"
    );
}

#[test]
fn overflow_no_left_chevron_at_first_tab() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = empty_app(Tab::Overview);
    let s = render_tabs(&app, 12);
    assert!(!s.contains('‹'), "no left chevron at first tab");
    assert!(
        s.contains('›'),
        "right chevron must appear when more tabs follow"
    );
}

#[test]
fn overflow_no_right_chevron_at_last_tab() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = empty_app(Tab::Plugin);
    let s = render_tabs(&app, 12);
    assert!(
        s.contains('‹'),
        "left chevron must appear for non-first tab"
    );
    assert!(!s.contains('›'), "no right chevron at last tab");
}

#[test]
fn truncate_label_plain() {
    assert_eq!(truncate_label("overview".into(), 5), "over…");
    assert_eq!(truncate_label("overview".into(), 8), "overview");
    assert_eq!(truncate_label("overview".into(), 0), "");
}

#[test]
fn truncate_label_preserves_suffix() {
    assert_eq!(truncate_label("my-long-name(45%)".into(), 10), "my-l…(45%)");
    assert_eq!(truncate_label("done(✓)".into(), 6), "do…(✓)");
}
