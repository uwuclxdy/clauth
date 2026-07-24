use super::*;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::status::{Impact, Incident, IncidentUpdate, UpdatePhase};
use crate::tui::app::{App, ConfigFocus, StatusFocus};
use crate::usage::{UsageInfo, UsageWindow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::collections::BTreeMap;

fn oauth(name: &str, five: f64, seven: f64, auto: bool) -> Profile {
    Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: auto,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(80.0),
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
            plan: None,
            five_hour: Some(UsageWindow {
                utilization: five,
                resets_at: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: seven,
                resets_at: None,
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn hybrid_creds() -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

fn demo_incidents() -> Vec<Incident> {
    vec![
        Incident {
            id: "6ptd5skgmy3v".into(),
            title: "opus 4.8 degraded service".into(),
            link: "https://stspg.io/abc".into(),
            phase: UpdatePhase::Monitoring,
            impact: Impact::Minor,
            started_ms: 1_780_740_881_000,
            resolved_ms: None,
            components: vec![
                ("claude.ai".into(), "operational".into()),
                ("Claude Code".into(), "degraded_performance".into()),
                ("Claude API".into(), "major_outage".into()),
            ],
            updates: vec![
                IncidentUpdate {
                    phase: UpdatePhase::Monitoring,
                    at_ms: 1_780_741_000_000,
                    text: "a fix has been implemented and we are monitoring".into(),
                    transitions: vec![
                        (
                            "claude.ai".into(),
                            "degraded_performance".into(),
                            "operational".into(),
                        ),
                        (
                            "Claude Code".into(),
                            "major_outage".into(),
                            "degraded_performance".into(),
                        ),
                    ],
                },
                IncidentUpdate {
                    phase: UpdatePhase::Investigating,
                    at_ms: 1_780_740_881_000,
                    text: "we are currently investigating this issue.".into(),
                    transitions: Vec::new(),
                },
            ],
        },
        Incident {
            id: "fprlnsvdnr2k".into(),
            title: "elevated errors on many claude models".into(),
            link: "https://stspg.io/def".into(),
            phase: UpdatePhase::Resolved,
            impact: Impact::None,
            started_ms: 1_780_684_084_000,
            resolved_ms: Some(1_780_686_904_000),
            components: Vec::new(),
            updates: vec![IncidentUpdate {
                phase: UpdatePhase::Resolved,
                at_ms: 1_780_686_904_000,
                text: "this incident has been resolved.".into(),
                transitions: Vec::new(),
            }],
        },
    ]
}

fn dump(app: &App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, app)).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .map(|r| r + "\n")
        .collect()
}

#[test]
fn a_two_line_toast_bolds_the_head_and_dims_the_detail() {
    use crate::tui::app::ToastKind;
    use ratatui::style::Modifier;

    // Centralized diagnostics render a `head\ndetail` body: head bold, detail
    // dim below it. No toast carried a detail line before, so this render path
    // (`toasts::draw`'s detail loop) was never exercised until now.
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.toast(ToastKind::Danger, "QHEAD\nQNOTE");

    let (w, h) = (80u16, 24u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let out = dump(&app, w, h);

    let modifier_at = |needle: &str| -> Modifier {
        let (row, line) = out
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` missing from toast:\n{out}"));
        let col = line.find(needle).unwrap();
        buf.content[row * (w as usize) + col].modifier
    };

    assert!(
        modifier_at("QHEAD").contains(Modifier::BOLD),
        "toast head is bold:\n{out}"
    );
    assert!(
        !modifier_at("QNOTE").contains(Modifier::BOLD),
        "toast detail line is dim, not bold:\n{out}"
    );
}

#[test]
fn login_modal_drops_the_url_and_offers_a_retry() {
    use crate::tui::app::{LoginSession, LoginStage, Modal, Tab};
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Setup;
    app.login = Some(LoginSession {
        name: "fresh".to_string(),
        is_new: true,
        generation: 1,
        url: Some("https://claude.com/cai/oauth/authorize?client_id=redacted".to_string()),
        stage: LoginStage::WaitingBrowser,
    });
    app.modals.push(Modal::Login);

    let out = dump(&app, 80, 24);
    assert!(
        out.contains("open the browser again"),
        "the login modal offers a browser retry:\n{out}",
    );
    assert!(
        !out.contains("oauth/authorize"),
        "the wrapped authorize URL is no longer rendered inline:\n{out}",
    );
}

#[test]
fn all_tabs_render() {
    let profiles = vec![
        oauth("uwuclxdy", 42.0, 18.0, true),
        oauth("work", 12.0, 3.0, false),
        oauth("spare", 0.0, 0.0, false),
    ];
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let config = AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: names.clone(),
            fallback_chain: vec!["uwuclxdy".into(), "work".into()],
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.status.incidents = demo_incidents();
    for (tab, focus) in [
        (Tab::Overview, ConfigFocus::Profiles),
        (Tab::Usage, ConfigFocus::Profiles),
        (Tab::Setup, ConfigFocus::Profiles),
        (Tab::Setup, ConfigFocus::Actions),
        (Tab::Fallback, ConfigFocus::Profiles),
        (Tab::Config, ConfigFocus::Profiles),
        (Tab::Status, ConfigFocus::Profiles),
    ] {
        app.tab = tab;
        app.config_focus = focus;
        assert!(dump(&app, 90, 20).contains("clauth"));
    }

    app.tab = Tab::Status;
    app.status.focus = StatusFocus::Detail;
    let detail = dump(&app, 90, 20);
    assert!(detail.contains("clauth"));
    assert!(
        detail.contains("TIMELINE"),
        "detail timeline eyebrow renders"
    );
    assert!(detail.contains("components"), "components row renders");
    assert!(detail.contains('→'), "a transition arrow renders");
    app.status.focus = StatusFocus::List;
}

#[test]
fn config_refresh_interval_custom_editor_renders() {
    use crate::tui::app::{GLOBAL_CONFIG_ROWS, GlobalConfigRow, InputState, Tab};
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::RefreshInterval)
        .unwrap();

    app.refresh_interval_draft = Some(InputState::new("45"));
    let valid = dump(&app, 90, 20);
    assert!(valid.contains("refresh"), "refresh row label renders");
    assert!(valid.contains("45"), "typed seconds render in the field");
    assert!(valid.contains("10-3600 s"), "valid-range tooltip renders");

    // An out-of-range buffer still renders (DANGER value) without panicking.
    app.refresh_interval_draft = Some(InputState::new("99999"));
    let invalid = dump(&app, 90, 20);
    assert!(invalid.contains("99999"));
    assert!(invalid.contains("10-3600 s"));

    // At rest, a custom (non-preset) interval shows the real value rather than
    // mis-bracketing the nearest preset chip.
    app.refresh_interval_draft = None;
    app.refresh_interval
        .store(200_000, std::sync::atomic::Ordering::Relaxed);
    let custom = dump(&app, 90, 20);
    assert!(
        custom.contains("200s"),
        "custom interval shows its real value"
    );
}

#[test]
fn fallback_threshold_editor_shows_range_tooltip() {
    use crate::tui::app::{FallbackFocus, InputState, Tab};
    let profiles = vec![oauth("uwuclxdy", 42.0, 18.0, true)];
    let config = AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: vec!["uwuclxdy".into()],
            fallback_chain: vec!["uwuclxdy".into()],
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.tab = Tab::Fallback;
    app.fallback_focus = FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = 0; // FALLBACK_ROWS[0] == `rotate at`

    // A valid in-range buffer shows the range tooltip (mirrors the refresh editor).
    app.fallback_threshold_draft = Some(InputState::new("70"));
    let valid = dump(&app, 90, 20);
    assert!(valid.contains("rotate at"), "threshold row label renders");
    assert!(
        valid.contains("70 %"),
        "typed value renders with the unit after the caret cell"
    );
    assert!(valid.contains("0-100 %"), "valid-range tooltip renders");

    // An out-of-range buffer still renders (DANGER) with the same range tooltip.
    app.fallback_threshold_draft = Some(InputState::new("150"));
    let invalid = dump(&app, 90, 20);
    assert!(invalid.contains("150"));
    assert!(invalid.contains("0-100 %"));
}

#[test]
fn setup_delete_row_hint_names_usage_history() {
    use crate::tui::app::{ConfigRow, Tab, config_rows};
    let profiles = vec![oauth("uwuclxdy", 42.0, 18.0, true)];
    let config = AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: vec!["uwuclxdy".into()],
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::Delete)
        .unwrap();

    // Delete removes the whole profile dir, usage_history.jsonl included, so the
    // hint must not read as if history survives (unlike delete-credentials).
    let out = dump(&app, 120, 30);
    assert!(
        out.contains("usage history included"),
        "delete-row hint names usage history:\n{out}",
    );
}

#[test]
fn setup_api_account_shows_relogin_and_logout_rows() {
    use crate::tui::app::{ConfigRow, Tab, config_rows};
    let mut api = oauth("acme", 0.0, 0.0, false);
    api.base_url = Some("https://api.example.com".to_string());
    api.api_key = Some("sk-secret".to_string());
    api.usage = None;
    let config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            ..AppState::default()
        },
        profiles: vec![api],
    };
    let mut app = App::new(config);
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    // Park the cursor on the login row so its API hint renders too.
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("api account shows a login row");

    let out = dump(&app, 120, 30);
    assert!(
        out.contains("re-login"),
        "api account with a key shows a re-login row:\n{out}",
    );
    assert!(
        out.contains("log out"),
        "api account with a key shows a log-out row:\n{out}",
    );
    assert!(
        out.contains("re-enter the base url"),
        "the login row's hint describes the API re-entry:\n{out}",
    );
}

/// A hybrid (stored OAuth pair + base url) with no api key: the login/log-out
/// rows must read off the credential that exists, or the tab reads "logged out"
/// over a live token.
#[test]
fn setup_hybrid_account_reads_logged_in_on_its_oauth_pair() {
    use crate::tui::app::{ConfigRow, Tab, config_rows};
    let mut hybrid = oauth("acme", 0.0, 0.0, false);
    hybrid.base_url = Some("http://127.0.0.1:1234".to_string());
    hybrid.credentials = Some(hybrid_creds());
    hybrid.usage = None;
    let config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            ..AppState::default()
        },
        profiles: vec![hybrid],
    };
    let mut app = App::new(config);
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    // Park on the login row (always present) so its hint renders too.
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("every account shows a login row");

    let out = dump(&app, 120, 30);
    assert!(
        out.contains("re-login"),
        "a stored OAuth pair reads as logged in:\n{out}",
    );
    assert!(
        out.contains("log out"),
        "a stored OAuth pair keeps the log-out row:\n{out}",
    );
    assert!(
        out.contains("browser OAuth login"),
        "the login row's hint describes the OAuth mint:\n{out}",
    );
}

/// Same typing rule for the log-out row's own hint: a hybrid holding both an api
/// key and an OAuth pair logs out of the pair, and the copy must say so.
#[test]
fn setup_hybrid_logout_hint_names_the_oauth_login() {
    use crate::tui::app::{ConfigRow, Tab, config_rows};
    let mut hybrid = oauth("acme", 0.0, 0.0, false);
    hybrid.base_url = Some("https://api.example.com".to_string());
    hybrid.api_key = Some("sk-secret".to_string());
    hybrid.credentials = Some(hybrid_creds());
    hybrid.usage = None;
    let config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            ..AppState::default()
        },
        profiles: vec![hybrid],
    };
    let mut app = App::new(config);
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::DeleteCreds)
        .expect("an api key alone already shows the log-out row");

    let out = dump(&app, 120, 30);
    assert!(
        out.contains("clears the stored OAuth"),
        "the log-out hint names the OAuth login it clears:\n{out}",
    );
}

#[test]
fn capture_name_caret_follows_edit_position() {
    use crate::actions::CaptureSnapshot;
    use crate::tui::app::{CaptureNameForm, InputState, Modal};

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });

    let mut input = InputState::new("alice");
    input.left();
    input.left(); // caret now sits before "ce", not at the end
    assert_ne!(input.cursor, input.value.len());

    app.modals.push(Modal::CaptureName(CaptureNameForm {
        snapshot: Box::new(CaptureSnapshot {
            credentials: None,
            base_url: None,
            api_key: None,
            identity: crate::actions::CaptureIdentity::LiveLogin,
        }),
        input,
        from_divergence: false,
    }));

    let (w, h) = (90u16, 20u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, &app)).unwrap();
    let mid_caret = term.get_cursor_position().unwrap();

    // Move the caret to the end of the same text and re-render: the terminal
    // caret column must shift right, i.e. it tracks `InputState::cursor`
    // instead of always snapping to the end of the string.
    if let Some(Modal::CaptureName(form)) = app.modals.last_mut() {
        form.input.end();
    }
    term.draw(|f| super::draw(f, &app)).unwrap();
    let end_caret = term.get_cursor_position().unwrap();

    assert_eq!(mid_caret.y, end_caret.y, "caret stays on the input row");
    assert!(
        mid_caret.x < end_caret.x,
        "caret column must follow the edit position (mid={mid_caret:?}, end={end_caret:?})"
    );
}

#[test]
fn status_selected_row_tint_spans_both_lines() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    let mut app = App::new(config);
    app.status.incidents = demo_incidents();
    app.tab = Tab::Status;
    app.status.focus = StatusFocus::List;
    app.status.cursor = 0;

    let (w, h) = (90u16, 20u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();

    let hover = crate::tui::theme::bg_hover();
    let cell = |x: u16, y: u16| &buf.content[(y as usize) * (w as usize) + (x as usize)];

    let mut caret_y = None;
    for y in 0..h {
        for x in 0..w {
            if cell(x, y).symbol() == "❯" {
                caret_y = Some(y);
                break;
            }
        }
        if caret_y.is_some() {
            break;
        }
    }
    let caret_y = caret_y.expect("selected incident renders a caret");

    let row_has_full_tint = |y: u16| {
        let caret_x = (0..w).find(|&x| cell(x, caret_y).symbol() == "❯").unwrap();
        let mut saw_filler_tint = false;
        for x in caret_x..w {
            let c = cell(x, y);
            if c.style().bg != Some(hover) {
                return saw_filler_tint && x > caret_x + 4;
            }
            if c.symbol() == " " {
                saw_filler_tint = true;
            }
        }
        saw_filler_tint
    };

    assert!(
        row_has_full_tint(caret_y),
        "title row tint must span the full content width"
    );
    assert!(
        row_has_full_tint(caret_y + 1),
        "pill row tint must span the full content width"
    );
}

// ── Narrow (phone-width) mode ────────────────────────────────────────────────
// A Moshi-class terminal is ~45 columns. These tests pin the two halves of the
// narrow contract: information survives at 45 cols (no truncation loss), and
// the narrow paths are width-gated (desktop renders keep their side-by-side
// shape). `╮╭` is the top-row junction two side-by-side boxes produce.

/// A loaded app for the narrow sweeps: profiles + chain + incidents + tokens.
fn narrow_app() -> App {
    use crate::tokens::{ModelTokens, TokenStats};

    let profiles = vec![
        oauth("uwuclxdy", 42.0, 18.0, true),
        oauth("work-account", 12.0, 3.0, false),
    ];
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let config = AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: names,
            fallback_chain: vec!["uwuclxdy".into(), "work-account".into()],
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.status.incidents = demo_incidents();
    app.token_stats = Some(TokenStats {
        models: vec![
            ModelTokens {
                model: "claude-opus-4-8".to_string(),
                input: 10_000_000,
                output: 800_000,
                ..Default::default()
            },
            ModelTokens {
                model: "claude-sonnet-5".to_string(),
                input: 4_000_000,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    app
}

/// The usage tab's "weekly limit is spent" teach keys on the HARD cap, not
/// the member line: 7d at 95 over a `weekly at 90` override is a switch
/// trigger, not a spent week — teaching "spent" there claims a state the
/// account isn't in. The override is what makes this discriminate: with no
/// override the member line IS the cap, and a folding revision stays green.
#[test]
fn usage_weekly_teach_keys_on_the_hard_cap_not_the_member_line() {
    use crate::usage::{FetchStatus, UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mk_app = |seven_day: f64| {
        let mut p = crate::testutil::blank_profile("a");
        p.weekly_threshold = Some(90.0);
        // 5h spent (past its rotate threshold, live reset) so the `spent`
        // pill — the teach's gate — renders at all.
        p.usage = Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 100.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
            }),
            seven_day: Some(UsageWindow {
                utilization: seven_day,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 5 * 86_400)),
            }),
            ..UsageInfo::default()
        });
        p.fetch_status = Some(FetchStatus::Fresh);
        let config = AppConfig {
            state: AppState {
                profiles: vec!["a".into()],
                ..AppState::default()
            },
            profiles: vec![p],
        };
        let mut app = App::new(config);
        app.tab = Tab::Usage;
        app
    };

    let over_line = dump(&mk_app(95.0), 100, 40);
    assert!(
        !over_line.contains("weekly limit is spent"),
        "95% over a 90 override still serves — no spent teach:\n{over_line}"
    );
    let at_cap = dump(&mk_app(100.0), 100, 40);
    assert!(
        at_cap.contains("weekly limit is spent"),
        "the literal cap earns the teach:\n{at_cap}"
    );
}

#[test]
fn narrow_master_detail_stacks_wide_stays_side_by_side() {
    let mut app = narrow_app();
    for tab in [
        Tab::Usage,
        Tab::Setup,
        Tab::Fallback,
        Tab::Status,
        Tab::Plugin,
        Tab::Tokens,
    ] {
        app.tab = tab;
        let narrow = dump(&app, 45, 38);
        assert!(
            !narrow.contains("╮╭"),
            "{tab:?} still renders side-by-side boxes at 45 cols:\n{narrow}"
        );
        let wide = dump(&app, 100, 30);
        assert!(
            wide.contains("╮╭"),
            "{tab:?} lost its desktop side-by-side layout at 100 cols:\n{wide}"
        );
    }
}

#[test]
fn narrow_tokens_dashboard_keeps_card_text_whole() {
    let mut app = narrow_app();
    app.tab = Tab::Tokens;
    let out = dump(&app, 45, 44);
    for needle in [
        "idle so far today",
        "cache hit",
        "COMPOSITION",
        "cache read",
    ] {
        assert!(
            out.contains(needle),
            "`{needle}` truncated away at 45 cols:\n{out}"
        );
    }
}

#[test]
fn narrow_overview_chain_row_keeps_its_figures_on_one_line() {
    let mut app = narrow_app();
    app.tab = Tab::Overview;
    let out = dump(&app, 45, 38);
    let row = out
        .lines()
        .find(|l| l.contains("1 uwuclxdy"))
        .unwrap_or_else(|| panic!("chain row missing:\n{out}"));
    assert!(
        row.contains("42") && row.contains("/ 80%"),
        "chain figures wrapped off the row at 45 cols:\n{out}"
    );

    // Wide keeps the full 12-cell gauge; narrow shrinks it instead of wrapping.
    let gauge_cells = |line: &str| line.chars().filter(|c| *c == '▰' || *c == '▱').count();
    assert_eq!(
        gauge_cells(row),
        10,
        "narrow gauge sized from leftover width"
    );
    let wide = dump(&app, 90, 24);
    let wide_row = wide
        .lines()
        .find(|l| l.contains("1 uwuclxdy"))
        .unwrap_or_else(|| panic!("wide chain row missing:\n{wide}"));
    assert_eq!(gauge_cells(wide_row), 12, "desktop gauge untouched");

    // The 3-digit worst case (a 100% last-resort member) is exactly the row
    // the old figure(4) budget overflowed by one column.
    let mut maxed = narrow_app();
    {
        let mut cfg = maxed.config();
        for p in cfg.profiles.iter_mut() {
            p.fallback_threshold = Some(100.0);
        }
    }
    maxed.tab = Tab::Overview;
    let out = dump(&maxed, 45, 38);
    let row = out
        .lines()
        .find(|l| l.contains("1 uwuclxdy"))
        .unwrap_or_else(|| panic!("100%-threshold chain row missing:\n{out}"));
    assert!(
        row.contains("/ 100%"),
        "3-digit threshold wrapped off the row at 45 cols:\n{out}"
    );
}

#[test]
fn narrow_footer_degrades_to_essential_hints() {
    let mut app = narrow_app();
    app.tab = Tab::Usage;
    let out = dump(&app, 45, 38);
    for needle in ["←→ tabs", "? help", "q quit"] {
        assert!(
            out.contains(needle),
            "essential hint `{needle}` missing at 45 cols:\n{out}"
        );
    }
    assert!(
        !out.contains("refresh account"),
        "non-essential hint should be dropped at 45 cols:\n{out}"
    );
    let wide = dump(&app, 120, 30);
    assert!(
        wide.contains("refresh account"),
        "desktop keeps the full hint set:\n{wide}"
    );
}

#[test]
fn narrow_header_hides_the_gauge_without_a_dangling_separator() {
    let mut app = narrow_app();
    app.tab = Tab::Usage;
    let narrow = dump(&app, 45, 38);
    let row = narrow
        .lines()
        .find(|l| l.contains("accounts"))
        .unwrap_or_else(|| panic!("header count row missing:\n{narrow}"));
    assert!(
        !row.contains('·'),
        "hidden gauge left a dangling separator:\n{row}"
    );
    let wide = dump(&app, 120, 30);
    let wide_row = wide
        .lines()
        .find(|l| l.contains("accounts"))
        .unwrap_or_else(|| panic!("wide header count row missing:\n{wide}"));
    assert!(
        wide_row.contains('·'),
        "desktop separator + gauge unchanged:\n{wide_row}"
    );
}

#[test]
fn narrow_status_detail_duration_drops_to_its_own_line() {
    let mut app = narrow_app();
    app.tab = Tab::Status;
    let out = dump(&app, 45, 38);
    // The duration must sit on its OWN line (bordered, otherwise empty), not
    // glued to the age at the end of the pill row.
    assert!(
        out.lines().any(|l| l.trim_matches([' ', '│']) == "ongoing"),
        "duration is not on its own line at 45 cols:\n{out}"
    );
}

#[test]
fn narrow_modal_body_wraps_instead_of_clipping() {
    use crate::tui::app::{ConfirmAction, ConfirmState, Modal};
    let mut app = narrow_app();
    app.modals.push(Modal::Confirm(ConfirmState {
        message: "switch every profile to a freshly rotated credential set immediately".into(),
        detail: None,
        choice: false,
        on_confirm: ConfirmAction::RotateAll,
    }));
    let out = dump(&app, 45, 38);
    assert!(
        out.contains("immediately"),
        "modal body clipped its tail at 45 cols:\n{out}"
    );
}

#[test]
fn empty_state_renders() {
    let config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    let mut app = App::new(config);
    for tab in Tab::ALL {
        app.tab = tab;
        assert!(dump(&app, 90, 20).contains("clauth"));
    }
}

#[test]
fn banner_renders() {
    use crate::tui::app::{Banner, BannerSeverity};

    let profiles = vec![oauth("alpha", 99.0, 50.0, true)];
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let config = AppConfig {
        state: AppState {
            active_profile: None,
            profiles: names,
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.banner = Some(Banner {
        severity: BannerSeverity::Danger,
        message: "all accounts spent · switch to an account to resume".to_string(),
    });

    let screen = dump(&app, 90, 20);
    assert!(screen.contains("all accounts spent"), "banner text missing");
    assert!(
        screen.contains("clauth"),
        "header missing with banner active"
    );

    app.banner = None;
    let screen_no_banner = dump(&app, 90, 20);
    assert!(
        !screen_no_banner.contains("all accounts spent"),
        "banner text present when banner is None"
    );
}

// A filter that empties the grouped model list (set from the menu while on the
// Models view) must explain itself — not fall back to the shared account
// empty-state ("no accounts yet · n to create one").
#[test]
fn tokens_models_view_empty_filter_names_the_filter() {
    use crate::tokens::{ModelTokens, TokenStats};
    use crate::tui::app::{TokenFilter, TokenView};

    let profiles = vec![oauth("uwuclxdy", 42.0, 18.0, true)];
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let config = AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: names,
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.tab = Tab::Tokens;
    app.token_view = TokenView::Models;
    app.token_filter = TokenFilter::Others;
    app.token_stats = Some(TokenStats {
        models: vec![ModelTokens {
            model: "claude-opus-4-8".to_string(),
            input: 10_000_000,
            ..Default::default()
        }],
        ..Default::default()
    });

    let out = dump(&app, 100, 30);
    assert!(
        out.contains("no models match the filter"),
        "filtered-empty models view must name the filter:\n{out}"
    );
    assert!(
        !out.contains("no accounts yet"),
        "must not fall back to the accounts empty state:\n{out}"
    );
}

// ── Fallback tab blocked-reason chip (weekly-fallback §4) ────────────────────

/// Bare OAuth profile with no usage snapshot — `blocked_reason` then keys purely
/// off `auth_broken`, so the chip tests need no live windows.
fn bare(name: &str) -> Profile {
    Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(95.0),
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

fn fallback_config(auth_broken: &[&str]) -> AppConfig {
    let names: Vec<ProfileName> = ["a", "b"].iter().map(|n| (*n).into()).collect();
    AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: names.clone(),
            fallback_chain: names,
            auth_broken: auth_broken.iter().map(|n| (*n).into()).collect(),
            ..AppState::default()
        },
        profiles: vec![bare("a"), bare("b")],
    }
}

// An auth-broken chain member carries its `×` marker in the selector list, so
// every ineligible member reads at a glance without opening its card. A chain
// with nothing blocked carries none.
#[test]
fn fallback_selector_marks_a_blocked_member() {
    let mut blocked = App::new(fallback_config(&["b"]));
    blocked.tab = Tab::Fallback;
    let out = dump(&blocked, 90, 20);
    assert!(
        out.contains('×'),
        "auth-broken b shows the × marker:\n{out}"
    );

    let mut healthy = App::new(fallback_config(&[]));
    healthy.tab = Tab::Fallback;
    let clean = dump(&healthy, 90, 20);
    assert!(
        !clean.contains('×'),
        "no marker when nothing is blocked:\n{clean}"
    );
}

// A typed threshold on a BLOCKED member must still place the native caret on the
// `rotate at` row: the blocked-reason pill block shifts every card row down and
// the cursor math has to follow. Asserted against the RENDERED row rather than a
// fixed delta — the block's height moved once already (it grew a fix line), and
// a delta test just re-derives the implementation's own arithmetic.
#[test]
fn fallback_edit_caret_follows_the_blocked_reason_pill() {
    let check = |auth_broken: &[&str], label: &str| {
        let mut app = App::new(fallback_config(auth_broken));
        app.tab = Tab::Fallback;
        app.chain_cursor = 0; // member a
        app.fallback_focus = crate::tui::app::FallbackFocus::Detail;
        app.fallback_detail_cursor = 0; // FallbackRow::Threshold
        app.fallback_threshold_draft = Some(crate::tui::app::InputState::new("90"));
        let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
        term.draw(|f| super::draw(f, &app)).unwrap();
        let caret = term.get_cursor_position().unwrap();
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let rendered_at = rows
            .iter()
            .position(|r| r.contains("rotate at"))
            .unwrap_or_else(|| panic!("[{label}] rotate at renders:\n{}", rows.join("\n")));
        assert_eq!(
            caret.y as usize, rendered_at,
            "[{label}] caret must sit on the rotate-at row (caret={}, row={rendered_at})",
            caret.y
        );
    };
    check(&[], "unblocked");
    check(&["a"], "blocked");
}

/// The Config and Setup panes rebuild their whole row list every frame into a
/// fixed-height `Paragraph`. Before they scrolled, anything past the viewport
/// vanished with no scrollbar and no clue — the last settings row and its hint
/// were simply absent at 24 rows. Both panes must keep the FOCUSED row (and its
/// tooltip) on screen at the smallest size the app renders at.
#[test]
fn form_panes_keep_the_focused_row_on_screen_when_they_overflow() {
    use crate::tui::app::{ConfigFocus, GLOBAL_CONFIG_ROWS, GlobalConfigRow, Tab};

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Config;

    // Cursor on the last row: it and its two-line hint sit past an 18-line
    // viewport, so only a scroll can reveal them.
    app.global_config_cursor = GLOBAL_CONFIG_ROWS.len() - 1;
    let bottom = dump(&app, 90, 24);
    assert!(
        bottom.contains("extra usage spent"),
        "the focused last row must not clip:\n{bottom}"
    );
    assert!(
        bottom.contains("extra usage runs out"),
        "its hint tooltip must not clip either:\n{bottom}"
    );
    assert!(
        bottom.contains('\u{2503}'),
        "an overflowing pane shows its scrollbar thumb:\n{bottom}"
    );

    // Cursor back near the top with a hinted row (so the pane still overflows):
    // the offset really tracks the cursor both ways — the first band header is
    // back AND the bottom row has gone off screen.
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::DivergenceDefault)
        .unwrap();
    let top = dump(&app, 90, 24);
    assert!(
        top.contains("APPEARANCE"),
        "the first band header is back on screen at the top:\n{top}"
    );
    assert!(
        !top.contains("extra usage spent"),
        "the offset really moved: the bottom row is off screen now:\n{top}"
    );

    // Setup's detail pane grows with a profile's env entries, so it overflows
    // the same way — its last row (`delete account`) must stay reachable.
    let mut p = oauth("uwuclxdy", 10.0, 10.0, false);
    for i in 0..8 {
        p.env.insert(format!("CUSTOM_VAR_{i}"), "value".into());
    }
    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: vec!["uwuclxdy".into()],
            ..AppState::default()
        },
        profiles: vec![p],
    });
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    app.config_action_cursor = usize::MAX; // clamped to the last row by `draw_settings`
    let setup = dump(&app, 90, 24);
    assert!(
        setup.contains("delete account"),
        "Setup's focused last row must not clip:\n{setup}"
    );
}

/// Each concern band is separated from the previous one by a blank row, so the
/// groups read as sections instead of one 12-row wall. The FIRST band opens the
/// pane, so it must not carry a leading spacer.
#[test]
fn config_bands_are_separated_by_one_blank_row() {
    use crate::tui::app::Tab;

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Config;
    app.global_config_cursor = 0;
    let screen = dump(&app, 90, 50);
    let lines: Vec<&str> = screen.lines().collect();
    let header = |label: &str| {
        lines
            .iter()
            .position(|l| l.contains(label))
            .unwrap_or_else(|| panic!("{label} renders:\n{screen}"))
    };

    for label in ["SCHEDULER", "AUTO-SWITCH", "EXTRA USAGE"] {
        let above = lines[header(label) - 1];
        assert!(
            !above.chars().any(char::is_alphanumeric),
            "{label} opens on a blank spacer row, got {above:?}:\n{screen}"
        );
    }
    let above_first = lines[header("APPEARANCE") - 1];
    assert!(
        above_first.contains("SETTINGS"),
        "the first band sits right under the panel title, no leading spacer: \
         {above_first:?}\n{screen}"
    );
}

/// A row's help tooltip wraps to the pane width, so a narrow pane turns a
/// one-line hint into four. Scrolling to the focused ROW is not enough — the
/// scroll has to clear the whole focused BLOCK, or the hint clips off the bottom
/// while the row it explains sits comfortably on screen.
#[test]
fn a_wrapped_hint_scrolls_into_view_with_its_row() {
    use crate::tui::app::{GLOBAL_CONFIG_ROWS, GlobalConfigRow, Tab};

    let mut app = App::new(AppConfig {
        state: AppState {
            // `allow extra usage` = pay-as-you-go carries the long ON hint, so its
            // wrapped block overflows a narrow pane and discriminates block-scroll.
            spend_budget_switching: true,
            ..AppState::default()
        },
        profiles: Vec::new(),
    });
    app.tab = Tab::Config;
    // `allow extra usage` carries a long hint and sits mid-pane, so the tail of
    // its wrapped block lands past the viewport without pulling the whole pane
    // to the end the way the last row would.
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::SpendBudget)
        .unwrap();

    // 26 cols is the widest terminal where this hint still wraps to a tail line
    // that lands past the viewport, so the assertion below keeps discriminating
    // block-scroll from row-scroll. `scroll_offset_keeps_the_whole_focused_block
    // _on_screen` pins the same rule off real copy.
    let screen = dump(&app, 26, 24);
    assert!(
        screen.contains("allow extra"),
        "the focused row renders:\n{screen}"
    );
    assert!(
        !screen.contains("APPEARANCE"),
        "the pane really scrolled: the first band is off the top:\n{screen}"
    );
    // The hint wraps to four lines ending in `its max spend`. That last line is
    // present only if the scroll cleared the block's tail, not just its row.
    assert!(
        screen
            .lines()
            .any(|l| l.trim_matches(['│', '┊', '┃', ' ']) == "its max spend"),
        "the hint's final wrapped line must not clip:\n{screen}"
    );
}

/// The block-vs-row scroll rule, pinned off the pure function instead of live
/// hint copy: the screen test above only discriminates while some real hint
/// happens to wrap past the viewport, so rewording one silently retires it.
#[test]
fn scroll_offset_keeps_the_whole_focused_block_on_screen() {
    use super::panes::scroll_offset;

    // Block 18..22 in a 20-row viewport. Scrolling to the ROW alone yields 1
    // (18 + 3 pad - 20) and clips the block's tail at 21.
    assert_eq!(scroll_offset(30, 20, (18, 22)), 5);
    // A block taller than the viewport caps at its first line, so the row it
    // explains survives even though its hint cannot.
    assert_eq!(scroll_offset(40, 10, (5, 30)), 5);
    // Content that fits never scrolls, whatever the focus.
    assert_eq!(scroll_offset(12, 20, (8, 11)), 0);
    // A block already on screen without scrolling stays put.
    assert_eq!(scroll_offset(30, 20, (2, 5)), 0);
}

/// End-to-end wiring for the reset-display setting (issue #39). The formatters
/// are unit-tested pure; only a real frame proves the operator's choice reaches
/// the usage-tab bar line — a call site left on `ResetFmt::default()` would keep
/// every other test green. Asserts a SHAPE, never a literal clock, so the test
/// holds in any timezone.
#[test]
fn usage_tab_reset_follows_the_reset_display_setting() {
    use crate::profile::ResetDisplay;
    use crate::tui::app::Tab;

    let mut profile = oauth("a", 40.0, 10.0, false);
    if let Some(w) = profile.usage.as_mut().and_then(|u| u.five_hour.as_mut()) {
        w.resets_at = Some(crate::usage::epoch_secs_to_iso(
            crate::usage::now_epoch_secs() + 40 * 60,
        ));
    }
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    });
    app.tab = Tab::Usage;

    // The stamp carries a `mon`-style day qualifier when the reset crosses
    // midnight (a 40m reset from late evening does), so accept both shapes.
    let stamped =
        regex::Regex::new(r"resets in \d+m \((?:[a-z]{3} )?\d\d:\d\d\)").expect("valid pattern");
    let countdown = regex::Regex::new(r"resets in \d+m").expect("valid pattern");
    // Pin both stamp shapes directly so the test holds at any wall clock, not
    // just when the fixture's 40m reset happens to stay same-day.
    assert!(stamped.is_match("resets in 9m (mon 00:09)"));
    assert!(stamped.is_match("resets in 9m (00:09)"));

    let relative = dump(&app, 100, 30);
    assert!(
        countdown.is_match(&relative),
        "the stock shape is a bare countdown:\n{relative}"
    );
    assert!(
        !stamped.is_match(&relative),
        "no clock renders until the operator asks for one:\n{relative}"
    );

    {
        let mut cfg = app.config();
        cfg.state.reset_display = Some(ResetDisplay::Both);
    }
    let both = dump(&app, 100, 30);
    assert!(
        stamped.is_match(&both),
        "`both` carries the countdown and a 24h stamp:\n{both}"
    );
}

/// The overview column is the one reset surface with a width budget, and no
/// frame test covered it — which is how a first cut shipped that DELETED the
/// countdown (and the 7d bar with it) at widths between the old and new tiers.
/// Opting into a clock must never render less than `relative` did at the same
/// width: worst case the stamp is dropped, never the countdown.
#[test]
fn overview_reset_column_never_loses_ground_to_the_clock_setting() {
    use crate::profile::ResetDisplay;
    use crate::tui::app::Tab;

    let mut profile = oauth("a", 40.0, 60.0, false);
    let now = crate::usage::now_epoch_secs();
    if let Some(u) = profile.usage.as_mut() {
        if let Some(w) = u.five_hour.as_mut() {
            w.resets_at = Some(crate::usage::epoch_secs_to_iso(now + 3 * 3600));
        }
        if let Some(w) = u.seven_day.as_mut() {
            w.resets_at = Some(crate::usage::epoch_secs_to_iso(now + 4 * 86400));
        }
    }
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    });
    app.tab = Tab::Overview;

    // The reset cell is whatever sits in parens after a bar's `%`, so this counts
    // it in every shape — `(3h 0m)`, `(22:50)`, `(thu 19:50)`, `(3h 0m · 22:50)`.
    let reset_cell = regex::Regex::new(r"% \([^)]+\)").expect("valid pattern");
    let stamped = regex::Regex::new(r"· \d\d:\d\d\)").expect("valid pattern");

    // Widths spanning every tier boundary the setting touches, including the
    // 81-90 and 102-111 bands where moving a threshold used to drop a column.
    for width in [85, 91, 100, 110, 112, 130, 160] {
        let relative = dump(&app, width, 20);
        let base = reset_cell.find_iter(&relative).count();

        for display in [ResetDisplay::Clock, ResetDisplay::Both] {
            {
                let mut cfg = app.config();
                cfg.state.reset_display = Some(display);
            }
            let with_clock = dump(&app, width, 20);
            let kept = reset_cell.find_iter(&with_clock).count();
            assert!(
                kept >= base,
                "at {width} cols {display:?} shows {kept} resets where relative showed \
                 {base}:\nrelative:\n{relative}\n{display:?}:\n{with_clock}"
            );
        }
        {
            let mut cfg = app.config();
            cfg.state.reset_display = None;
        }
    }

    // And the wide tier genuinely carries a stamp on BOTH columns once the
    // terminal can pay for it — the point of the setting.
    {
        let mut cfg = app.config();
        cfg.state.reset_display = Some(ResetDisplay::Both);
    }
    let wide = dump(&app, 160, 20);
    assert_eq!(
        stamped.find_iter(&wide).count(),
        2,
        "a 160-col overview stamps both the 5h and 7d resets:\n{wide}"
    );
}
