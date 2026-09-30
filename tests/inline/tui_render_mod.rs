#![allow(clippy::unwrap_used, clippy::expect_used)]

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
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: auto,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(80.0),
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
        usage_stale: false,
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
            ..crate::profile::OAuthToken::default_extra()
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
fn hybrid_renders_the_activity_and_deadline_of_its_provider_cache() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::profile::{ClaudeCredentials, OAuthToken};
    use crate::tui::app::Tab;
    use crate::usage::{FetchLeg, ProfileActivity, mark_activity, mark_fetch_activity};

    let mut hybrid = oauth("hybrid", 40.0, 60.0, false);
    hybrid.base_url = Some("https://api.deepseek.com".to_string());
    hybrid.api_key = Some("key".to_string());
    hybrid.provider = crate::providers::Provider::from_base_url("https://api.deepseek.com");
    hybrid.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    let name = hybrid.name.clone();
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![name.clone()],
            ..AppState::default()
        },
        profiles: vec![hybrid],
    });
    let now = crate::usage::now_ms();
    app.next_refresh_per_profile
        .lock()
        .unwrap()
        .insert(FetchLeg::OAuth.key(name.clone()), now + 11_000);
    app.next_refresh_per_profile
        .lock()
        .unwrap()
        .insert(FetchLeg::ThirdParty.key(name.clone()), now + 222_000);

    mark_activity(&app.activity, &name, ProfileActivity::Fetching);
    app.tab = Tab::Usage;
    let oauth_only_usage = dump(&app, 100, 24);
    assert!(
        (oauth_only_usage.contains("refresh in 221s")
            || oauth_only_usage.contains("refresh in 222s"))
            && !oauth_only_usage.contains("refresh in 11s"),
        "provider figures keep their provider countdown while OAuth fetches:\n{oauth_only_usage}"
    );
    app.tab = Tab::Overview;
    let oauth_only_overview = dump(&app, 100, 24);
    assert!(
        oauth_only_overview.contains("221s") || oauth_only_overview.contains("222s"),
        "overview keeps the provider countdown while OAuth fetches:\n{oauth_only_overview}"
    );

    mark_fetch_activity(
        &app.activity,
        &FetchLeg::ThirdParty.key(name),
        ProfileActivity::Fetching,
    );
    for tab in [Tab::Usage, Tab::Overview] {
        app.tab = tab;
        let provider_fetch = dump(&app, 100, 24);
        assert!(
            provider_fetch.contains(crate::spinner::SPINNER_FRAMES[0]),
            "the provider fetch replaces its own countdown on {tab:?}:\n{provider_fetch}"
        );
    }
}

#[test]
fn a_two_line_toast_bolds_the_head_and_dims_the_detail() {
    let _home = crate::testutil::HomeSandbox::new();
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

/// An in-flight OAuth login at the waiting stage with its paste door open, plus
/// the receiver its worker would hold (dropping it would close the door). The
/// links are built by hand: nothing here needs a bound listener.
fn paste_session() -> (
    crate::tui::app::LoginSession,
    std::sync::mpsc::Receiver<crate::oauth_login::ManualCode>,
) {
    use crate::tui::app::{LoginMethod, LoginSession, LoginStage, PasteDoor};
    let (tx, rx) = std::sync::mpsc::channel();
    let session = LoginSession {
        name: "fresh".to_string(),
        is_new: true,
        generation: 1,
        url: Some("https://claude.com/cai/oauth/authorize?client_id=redacted".to_string()),
        stage: LoginStage::WaitingBrowser,
        method: LoginMethod::Browser,
        paste: Some(PasteDoor {
            links: crate::oauth_login::LoginLinks {
                browser_url: "https://claude.com/cai/oauth/authorize?client_id=redacted"
                    .to_string(),
                hosted_url: "https://claude.com/cai/oauth/authorize?redirect_uri=hosted"
                    .to_string(),
                state: "fixture-state".to_string(),
            },
            tx,
        }),
        paste_field: None,
    };
    (session, rx)
}

#[test]
fn login_modal_drops_the_url_and_offers_a_retry() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::{Modal, Tab};
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Setup;
    let (session, _rx) = paste_session();
    app.login = Some(session);
    app.modals.push(Modal::Login);

    let out = dump(&app, 80, 24);
    assert!(
        out.contains("open the browser again"),
        "the login modal offers a browser retry:\n{out}",
    );
    assert!(
        !out.contains("oauth/authorize"),
        "neither authorize URL is rendered inline:\n{out}",
    );
}

#[test]
fn all_tabs_render() {
    let _home = crate::testutil::HomeSandbox::new();
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

/// The status tab's incident stamps render LOCAL wall clock in the 2026-08-22
/// ruling's `YYYY-MM-DD HH:MM:SS` shape — the `started` row and every timeline
/// row. Both used to slice the UTC ISO (`jun 6, 10:14`), and a bare stamp in
/// prose reads as local, so the unmarked rows were false off UTC. The expected
/// strings derive through chrono's own `format` rather than the crate's
/// formatter, so the pin is a second derivation of the same claim, not a copy
/// of the code's arithmetic; it holds in any zone, UTC included (local equals
/// UTC there, and the contract is satisfied by either).
#[test]
fn status_tab_incident_stamps_render_local_wall_clock() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::Tab;

    let incidents = demo_incidents();
    let started_ms = incidents[0].started_ms;
    let update_ms: Vec<u64> = incidents[0].updates.iter().map(|u| u.at_ms).collect();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.status.incidents = incidents;
    app.tab = Tab::Status;
    app.status.focus = StatusFocus::Detail;
    let out = dump(&app, 90, 24);

    let local_shape = |epoch_ms: u64| {
        chrono::DateTime::from_timestamp((epoch_ms / 1000) as i64, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    };

    let started = local_shape(started_ms);
    assert!(
        out.contains(&started),
        "started row renders the local stamp {started:?}:\n{out}"
    );
    for at_ms in update_ms {
        let row = local_shape(at_ms);
        assert!(
            out.contains(&row),
            "timeline row renders the local stamp {row:?}:\n{out}"
        );
    }
    assert!(
        !out.contains(" utc"),
        "no stamp carries a utc marker after the local conversion:\n{out}"
    );

    // Where the zone spells the fixture instant differently from UTC, the
    // UTC digits must be ABSENT: a stamp that regressed to UTC in the 19-char
    // shape would still satisfy a contains() on a UTC box. Compared
    // spelling-on-spelling, never via a run-instant offset read: a DST zone
    // whose offset differs between the fixture instant and the run instant
    // would fire the guard over two identical spellings. On a UTC runner the
    // two spellings coincide, so there is no second spelling to ban.
    let utc_started = chrono::DateTime::from_timestamp((started_ms / 1000) as i64, 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    if started != utc_started {
        assert!(
            !out.contains(&utc_started),
            "the started stamp renders UTC digits, not local wall clock:\n{out}"
        );
    }
}

#[test]
fn config_refresh_interval_custom_editor_renders() {
    let _home = crate::testutil::HomeSandbox::new();
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
fn config_context_nudge_custom_editor_renders() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::{GLOBAL_CONFIG_ROWS, GlobalConfigRow, InputState, Tab};
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::ContextNudge)
        .unwrap();

    app.context_nudge_draft = Some(InputState::new("600k"));
    let valid = dump(&app, 90, 20);
    assert!(valid.contains("context nudge"), "nudge row label renders");
    assert!(valid.contains("600k"), "typed tokens render in the field");
    assert!(
        valid.contains("50k-2M tokens"),
        "valid-range tooltip renders"
    );

    // An out-of-range buffer still renders (DANGER value) without panicking.
    app.context_nudge_draft = Some(InputState::new("49999"));
    let invalid = dump(&app, 90, 20);
    assert!(invalid.contains("49999"));
    assert!(invalid.contains("50k-2M tokens"));
}

#[test]
fn fallback_threshold_editor_shows_range_tooltip() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::{CardEdit, FallbackFocus, InputState, MemberEdit, Tab};
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
    app.fallback_edit = Some(MemberEdit {
        member: "uwuclxdy".into(),
        state: CardEdit::Threshold(InputState::new("70")),
    });
    let valid = dump(&app, 90, 20);
    assert!(valid.contains("rotate at"), "threshold row label renders");
    assert!(
        valid.contains("70 %"),
        "typed value renders with the unit after the caret cell"
    );
    assert!(valid.contains("0-100 %"), "valid-range tooltip renders");

    // An out-of-range buffer still renders (DANGER) with the same range tooltip.
    app.fallback_edit = Some(MemberEdit {
        member: "uwuclxdy".into(),
        state: CardEdit::Threshold(InputState::new("150")),
    });
    let invalid = dump(&app, 90, 20);
    assert!(invalid.contains("150"));
    assert!(invalid.contains("0-100 %"));
}

#[test]
fn setup_delete_row_hint_names_usage_history() {
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
        out.contains("re-enter the base URL"),
        "the login row's hint describes the API re-entry:\n{out}",
    );
}

/// The Setup picker ends in exactly one trailing row (`+ new`), and the
/// create form carries `+ capture current login` under `+ login` while the
/// live login is unowned — flipping to its ✓ done state once stashed, and
/// gone entirely when no live login is unsaved.
#[test]
fn new_form_renders_the_capture_row_and_its_done_state() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::actions::CaptureSnapshot;
    use crate::tui::app::{ConfigRow, DraftLogin, Tab, build_draft_new, config_rows};

    // A plain live credentials file, as `claude` itself leaves it.
    let live = crate::profile::claude_dir()
        .expect("claude dir")
        .join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(
        &live,
        serde_json::to_vec(&crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "live-access".to_string(),
                refresh_token: Some("live-refresh".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
                ..crate::profile::OAuthToken::default_extra()
            }),
        })
        .expect("serialize live login"),
    )
    .expect("write live login");

    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![],
    };
    let mut app = App::new(config);
    app.refresh_unsaved_live_login();
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    app.profile_cursor = 0; // the `+ new` form

    // Park the cursor on the capture row so its hint renders too.
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::CaptureLogin)
        .expect("the capture row renders for an unowned live login");

    let out = dump(&app, 120, 30);
    assert!(
        !out.contains("+ new from"),
        "the picker carries no second trailing row:\n{out}"
    );
    assert!(
        out.contains("+ capture current login"),
        "the form carries the capture row under `+ login`:\n{out}"
    );
    assert!(
        out.contains("save the current global credentials into a new account"),
        "the capture row's hint explains what ⏎ stashes:\n{out}"
    );

    // Stashed: the ✓ done state, same pattern as `✓ logged in`.
    let mut draft = build_draft_new();
    draft.captured_login = Some(DraftLogin::LiveLogin(Box::new(CaptureSnapshot {
        credentials: None,
        base_url: None,
        api_key: None,
        account_uuid: None,
    })));
    app.config_draft = Some(draft);
    let out = dump(&app, 120, 30);
    assert!(
        out.contains("✓ captured current login"),
        "a stashed snapshot renders the done state:\n{out}"
    );

    // No unsaved live login: no row at all.
    app.config_draft = None;
    app.unsaved_live_login = false;
    let out = dump(&app, 120, 30);
    assert!(
        !out.contains("capture current login"),
        "a saved or absent live login hides the row:\n{out}"
    );
}

/// A hybrid (stored OAuth pair + base url) with no api key: the login/log-out
/// rows must read off the credential that exists, or the tab reads "logged out"
/// over a live token.
#[test]
fn setup_hybrid_account_reads_logged_in_on_its_oauth_pair() {
    let _home = crate::testutil::HomeSandbox::new();
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
        out.lines()
            .any(|l| l.trim_matches(|c| c == '│' || c == ' ') == "└ browser OAuth login"),
        "the login row's hint is the OAuth mint's three words, whole:\n{out}",
    );
}

/// Same typing rule for the log-out row's own hint: a hybrid holding both an api
/// key and an OAuth pair logs out of the pair, and the copy must say so.
#[test]
fn setup_hybrid_logout_hint_names_the_oauth_login() {
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
            account_uuid: None,
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
    use crate::usage::{FetchStatus, UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mk_app = |seven_day: f64| {
        let mut p = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
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
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = narrow_app();
    for tab in [
        Tab::Usage,
        Tab::Setup,
        Tab::Fallback,
        Tab::Status,
        Tab::Services,
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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

/// The footer advertises `a` only where the menu has something in it. The
/// Setup tab's three actions all work on the focused account and the `+ new`
/// form carries only `apply preset` (no source account to duplicate or save),
/// each beside the daemon verb, so the hint stays up in both positions; the
/// one empty menu left, a tab with no action of its own while a daemon start
/// or stop holds that verb back, drops it.
#[test]
fn the_actions_hint_tracks_whether_the_menu_has_anything_in_it() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("acct")],
            ..AppState::default()
        },
        profiles: vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("acct"),
        )],
    });
    app.tab = Tab::Setup;
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let menu = crate::tui::app::build_action_menu(&app);
    assert_eq!(menu.items.len(), 4);
    assert_eq!(
        menu.context.as_deref(),
        Some("acct"),
        "a scoped group names the account it scopes to"
    );
    let out = dump(&app, 120, 30);
    assert!(
        out.contains("a actions"),
        "an account carries three whole-account actions no key reaches:\n{out}"
    );

    // The create form sits past the roster: only `apply preset` is offered
    // (no source account to duplicate or save), but that still means the menu
    // has an item, so the footer keeps advertising `a`.
    app.profile_cursor = app.profile_count();
    app.config_draft = None;
    let menu = crate::tui::app::build_action_menu(&app);
    assert_eq!(menu.items.len(), 2);
    assert_eq!(
        menu.context, None,
        "no draft is mounted yet, so the group title is bare"
    );
    let out = dump(&app, 120, 30);
    assert!(
        out.contains("a actions"),
        "the create form's menu carries apply preset, so the key stays advertised:\n{out}"
    );

    app.tab = Tab::Config;
    app.daemon_control_busy = true;
    assert!(crate::tui::app::build_action_menu(&app).items.is_empty());
    let out = dump(&app, 120, 30);
    assert!(
        !out.contains("a actions"),
        "an empty menu is never advertised:\n{out}"
    );
    app.daemon_control_busy = false;
    let out = dump(&app, 120, 30);
    assert!(
        out.contains("a actions"),
        "the daemon verb alone keeps it advertised:\n{out}"
    );
    app.tab = Tab::Services;
    let out = dump(&app, 120, 30);
    assert!(
        out.contains("a actions"),
        "the Services tab's menu holds the daemon verb too:\n{out}"
    );
}

#[test]
fn narrow_footer_degrades_to_essential_hints() {
    let _home = crate::testutil::HomeSandbox::new();
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
fn narrow_status_detail_duration_drops_to_its_own_line() {
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(95.0),
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
    let check = |auth_broken: &[&str], label: &str| {
        let mut app = App::new(fallback_config(auth_broken));
        app.tab = Tab::Fallback;
        app.chain_cursor = 0; // member a
        app.fallback_focus = crate::tui::app::FallbackFocus::Detail;
        app.fallback_detail_cursor = 0; // FallbackRow::Threshold
        app.fallback_edit = Some(crate::tui::app::MemberEdit {
            member: "a".into(),
            state: crate::tui::app::CardEdit::Threshold(crate::tui::app::InputState::new("90")),
        });
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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

/// The `home tab` row is the Config tab's widest (eight chips, 87 cells
/// focused), so at a narrow pane the chip run breaks BETWEEN chips onto
/// continuation lines indented to the value column — the contract's
/// multi-select wrapping clause, extended to the cycle row. The selected value
/// must stay visible wherever it sits in the run.
#[test]
fn the_home_tab_row_wraps_between_chips_at_a_narrow_pane() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::profile::HomeTab;
    use crate::tui::app::{GLOBAL_CONFIG_ROWS, GlobalConfigRow, Tab};

    let mut app = App::new(AppConfig {
        state: AppState {
            home_tab: Some(HomeTab::Services),
            ..AppState::default()
        },
        profiles: Vec::new(),
    });
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::HomeTab)
        .expect("home tab row");

    // 60 cols: the first line holds the key cell plus four chips, the rest of
    // the run — including the selected `[services]` — continues below.
    let screen = dump(&app, 60, 24);
    assert!(
        screen.contains("home tab"),
        "the home tab row renders:\n{screen}"
    );
    assert!(
        screen.contains("[services]"),
        "the selected value must stay visible at a narrow pane:\n{screen}"
    );
    assert!(
        screen.lines().any(|l| {
            // The value column: `2 + KEY_W + KEY_GUTTER` (17 + 2), the
            // row's lead; the dump's border column adds one space.
            // Trimming the spaces here would let a column-0 continuation pass.
            let t = l.trim_start_matches(['│', '┊', '┃']);
            let t = t.strip_prefix(' ').unwrap_or(t);
            t.starts_with(&format!("{}fallback", " ".repeat(21)))
        }),
        "the chip run must continue between chips on a line indented to the value column:\n{screen}"
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
    let _home = crate::testutil::HomeSandbox::new();
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

/// The overview accounts table's width budget, pinned at the FRAME across every
/// tier boundary the column algorithm has. Every new column plays by the reset
/// column's rule — it may spend leftover slack and it may be dropped whole, but
/// it may never cost a bar or a reset at any width — and the only way to catch a
/// breach is to count what a real render puts on screen.
///
/// The expected counts are the behavior BEFORE the `live` column existed, so a
/// column that pays for itself out of a tier instead of out of slack reds here
/// rather than silently deleting a bar at some width nobody tests by hand (which
/// is exactly what the 2026-07-19 reset-column cut did).
#[test]
fn the_overview_column_budget_never_pays_for_a_new_column_with_an_old_one() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::Tab;

    // A bar is 10 interior cells in brackets, always trailed by its percentage —
    // specific enough that no pill or chip elsewhere in the frame can match it.
    let bar = regex::Regex::new(r"\[[^\[\]]{10}\] +\d+%").expect("valid pattern");
    let reset_cell = regex::Regex::new(r"% \([^)]+\)").expect("valid pattern");

    // Pre-column behavior as a function of TERMINAL width, per NAME LENGTH. The
    // name sweep is what makes this pin able to fail: `OverviewWidths` clamps
    // `max_name` to 8 at the low end, so a one-profile-named-"a" fixture never
    // enters the shrink loop's name arm and never reaches the width where the 7d
    // column is what gives way. 16 and 22 do both.
    //
    // Boundaries are terminal widths, not the tiers spelled in `OverviewWidths`
    // (the accounts area is narrower by the frame's own chrome). The 5h bar
    // arrives, then its reset, then the 7d column brings its own pair — one
    // column later for a 22-char name, which is the shrink loop trading.
    const BUDGET: &[(usize, u16, u16, u16, u16)] = &[
        // (name length, first bar, first reset, 7d bar, 7d reset)
        (8, 68, 85, 106, 106),
        (16, 68, 85, 106, 106),
        (22, 68, 85, 106, 108),
    ];

    for (name_len, first_bar, first_reset, seven_bar, seven_reset) in BUDGET {
        let name = "n".repeat(*name_len);
        let mut profile = oauth(&name, 40.0, 60.0, false);
        let now = crate::usage::now_epoch_secs();
        if let Some(u) = profile.usage.as_mut() {
            // Both resets sit MID-unit, deliberately. `humanize_duration` narrows
            // by a cell the moment a countdown crosses a unit boundary (`3h 0m` →
            // `2h 59m`, `4d 0h` → `3d 23h`), and `reset_suffix` fit-checks the
            // stamp — so a fixture pinned exactly ON a boundary decides a column
            // boundary by which second the render lands in. That shipped as a
            // 28%-flaky pin at the 22-char row's 107th column.
            if let Some(w) = u.five_hour.as_mut() {
                w.resets_at = Some(crate::usage::epoch_secs_to_iso(now + 3 * 3600 + 1800));
            }
            if let Some(w) = u.seven_day.as_mut() {
                w.resets_at = Some(crate::usage::epoch_secs_to_iso(now + 4 * 86400 + 43200));
            }
        }
        let mut app = App::new(AppConfig {
            state: AppState::default(),
            profiles: vec![profile],
        });
        app.tab = Tab::Overview;

        for width in 34u16..=200 {
            let frame = dump(&app, width, 20);
            let bars = usize::from(width >= *first_bar) + usize::from(width >= *seven_bar);
            let resets = usize::from(width >= *first_reset) + usize::from(width >= *seven_reset);
            assert_eq!(
                (
                    bar.find_iter(&frame).count(),
                    reset_cell.find_iter(&frame).count()
                ),
                (bars, resets),
                "a {name_len}-char name at {width} cols must still carry {bars} bars \
                 and {resets} resets:\n{frame}"
            );
        }
    }
}

/// The overview column is the one reset surface with a width budget, and no
/// frame test covered it — which is how a first cut shipped that DELETED the
/// countdown (and the 7d bar with it) at widths between the old and new tiers.
/// Opting into a clock must never render less than `relative` did at the same
/// width: worst case the stamp is dropped, never the countdown.
#[test]
fn overview_reset_column_never_loses_ground_to_the_clock_setting() {
    let _home = crate::testutil::HomeSandbox::new();
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

/// Whether a real frame carries the accounts table's live-session column, read
/// off the table's own HEADER ROW rather than the whole frame.
///
/// A frame-wide `.contains("live")` would be vacuous: `live` is a four-character
/// needle and the chain panel, the captions and the footer are all prose on the
/// same screen, so any of them saying "live" would report the column present at
/// every width, green. The header row is located by the two labels that always
/// precede this one (`account` and `type`, both of which survive the narrowest
/// tier), and a missing row PANICS rather than reading as absence — an absence
/// claim from a probe that never ran is worth nothing.
fn live_column_present(app: &App, width: u16) -> bool {
    let frame = dump(app, width, 20);
    let header = frame
        .lines()
        .find(|row| row.contains("account") && row.contains("type"))
        .unwrap_or_else(|| panic!("no accounts-table header at {width} cols:\n{frame}"));
    // Whole-token, so a column clipped mid-label ("liv") reads as absent — which
    // is the point: the tail ratatui discards IS this column. Tokenizing also
    // steps over the panel's own `│` border cells, which sit on the same row.
    header.split_whitespace().any(|cell| cell == "live")
}

/// Where the `live` column exists, exactly, as a function of name length and
/// terminal width. The column is monotone in width: once it is present at some
/// width it is present at every wider width, so each map is one `(floor, 200)`
/// range and the column never disappears as the terminal widens.
///
/// The fit gate's own comparison had no pin: `<= total` → `< total` shifts every
/// arrival by one column and the whole suite stayed green, because the only test
/// watching presence reads `widths.live` and then checks the render agrees with
/// it, which is true however the gate is written. This asserts the map instead,
/// so the boundary is a fact rather than a self-consistency check.
///
/// The column is decided before the wall-clock stamp bonus, so it outranks the
/// stamp's clock cells rather than the other way round: the map is identical
/// whether `reset_display` shows a clock or not, and only the stamp's wall-clock
/// half may lose cells to it.
///
/// The floors are TERMINAL widths. The width math itself runs in the list-area
/// inner width, 4 cells narrower — the accounts panel's rounded border plus a
/// 1-cell horizontal padding on each side — so the pure-function floors read
/// 4 lower than these.
#[test]
fn the_live_column_appears_monotonically_in_width() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::Tab;

    // (name length, terminal width at which the column first appears)
    const FLOORS: &[(usize, u16)] = &[(8, 48), (12, 52), (13, 71), (14, 107), (16, 109), (22, 115)];

    for (name_len, floor) in FLOORS {
        let name = "n".repeat(*name_len);
        for clock in [false, true] {
            let state = AppState {
                reset_display: if clock {
                    Some(crate::profile::ResetDisplay::Both)
                } else {
                    None
                },
                ..Default::default()
            };
            let mut app = App::new(AppConfig {
                state,
                profiles: vec![oauth(&name, 40.0, 60.0, false)],
            });
            app.tab = Tab::Overview;

            let present: Vec<u16> = (34u16..=200)
                .filter(|w| live_column_present(&app, *w))
                .collect();
            let expected: Vec<u16> = (*floor..=200).collect();
            assert_eq!(
                present, expected,
                "the {name_len}-char presence map moved (clock {clock})",
            );
        }
    }
}

// ── the login modal's two doors ──────────────────────────────────────────────

/// The row index of the first line containing `needle`, so row ORDER can be
/// asserted, not just presence.
fn row_of(out: &str, needle: &str) -> usize {
    out.lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("`{needle}` missing:\n{out}"))
}

/// The last frame row: the footer's login line.
fn footer_of(out: &str) -> &str {
    out.lines().last().unwrap_or("").trim()
}

/// While an OAuth login waits it offers both doors — the browser retry, the
/// link for another device, the paste row — in that order under the one
/// waiting text both flows share; no browser blurb, and the link itself is
/// never drawn. Once the paste door won (the worker's `ExchangingCode(Manual)`)
/// the rows go and the stage line says so, with no browser copy anywhere in the
/// frame. The footer carries the name and the key hint alone, in both states.
#[test]
fn login_modal_offers_both_doors_while_waiting_then_only_the_stage() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::{LoginMethod, LoginStage, Modal, Tab};
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Setup;
    let (session, _rx) = paste_session();
    app.login = Some(session);
    app.modals.push(Modal::Login);
    let footer = format!(
        "{} logging in 'fresh'   q back",
        super::format::spinner_frame(app.tick_count)
    );

    let out = dump(&app, 100, 30);
    let stage = row_of(&out, "continue in your browser");
    let r = row_of(&out, "r  open the browser again");
    let c = row_of(&out, "c  copy link");
    let p = row_of(&out, "p  paste code");
    assert!(
        stage < r && r + 1 == c && c + 1 == p,
        "rows in order:\n{out}"
    );
    assert!(
        !out.contains("complete the login"),
        "no browser blurb; the stage line is the whole waiting text:\n{out}"
    );
    assert!(
        !out.contains("oauth/authorize"),
        "no authorize URL is rendered:\n{out}"
    );
    assert_eq!(footer_of(&out), footer, "the footer: name + hint, no stage");

    let session = app.login.as_mut().expect("session");
    session.stage = LoginStage::ExchangingCode(LoginMethod::Manual);
    session.method = LoginMethod::Manual;
    let out = dump(&app, 100, 30);
    assert!(
        out.contains("logging in with the code"),
        "the stage line names the paste exchange:\n{out}"
    );
    for gone in [
        "open the browser again",
        "copy link",
        "paste code",
        "continue in your browser",
    ] {
        assert!(
            !out.contains(gone),
            "`{gone}` is gone once a door landed:\n{out}"
        );
    }
    assert!(
        !out.to_lowercase().contains("browser"),
        "no browser copy anywhere in the frame, footer included:\n{out}"
    );
    assert_eq!(
        footer_of(&out),
        footer,
        "the footer never carries the stage"
    );

    // The browser door's later stages keep their own two texts.
    let session = app.login.as_mut().expect("session");
    session.stage = LoginStage::ExchangingCode(LoginMethod::Browser);
    session.method = LoginMethod::Browser;
    let out = dump(&app, 100, 30);
    assert!(out.contains("exchanging the code for tokens"), "{out}");
    session_stage(&mut app, LoginStage::Verifying);
    let out = dump(&app, 100, 30);
    assert!(out.contains("verifying the minted token"), "{out}");
}

fn session_stage(app: &mut App, stage: crate::tui::app::LoginStage) {
    if let Some(s) = app.login.as_mut() {
        s.stage = stage;
    }
}

/// The console login has one door: `r` alone under the same waiting text, no
/// browser blurb, and a footer that offers no paste.
#[test]
fn the_console_login_modal_keeps_r_alone() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::{LoginMethod, LoginSession, LoginStage, Modal, Tab};
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Setup;
    app.login = Some(LoginSession {
        name: "qwen".to_string(),
        is_new: false,
        generation: 1,
        url: Some("https://account.alibabacloud.com/login".to_string()),
        stage: LoginStage::WaitingBrowser,
        method: LoginMethod::Browser,
        paste: None,
        paste_field: None,
    });
    app.modals.push(Modal::Login);

    let out = dump(&app, 100, 30);
    let stage = row_of(&out, "continue in your browser");
    let r = row_of(&out, "r  open the browser again");
    assert!(stage < r, "the stage line leads the row:\n{out}");
    for absent in ["complete the login", "copy link", "paste code"] {
        assert!(!out.contains(absent), "`{absent}` is not offered:\n{out}");
    }
    assert_eq!(
        footer_of(&out),
        format!(
            "{} logging in 'qwen'   q back",
            super::format::spinner_frame(app.tick_count)
        ),
        "the footer: name + hint, no stage"
    );
}

/// The code field stands in for the `p  paste code` row: the placeholder while
/// empty, the typed text verbatim once there is any (never a mask, never a
/// count), the stage line reading `pasting the code`, the native caret on the
/// value, and a footer of `↵ submit   esc back` with no parenthetical. A field
/// left set after the door closed renders neither the row nor its stage text.
#[test]
fn the_code_field_shows_the_typed_text_and_the_placeholder() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::{InputState, LoginMethod, LoginStage, Modal, Tab};
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Setup;
    let (session, _rx) = paste_session();
    app.login = Some(session);
    app.modals.push(Modal::Login);
    let footer = format!(
        "{} logging in 'fresh'   ↵ submit   esc back",
        super::format::spinner_frame(app.tick_count)
    );

    session_field(&mut app, Some(InputState::new("")));
    let out = dump(&app, 80, 24);
    let stage = row_of(&out, "pasting the code");
    let c = row_of(&out, "c  copy link");
    let field = row_of(&out, "✎ code (paste it here)");
    assert!(
        stage < c && c + 1 == field,
        "the field is the third row:\n{out}"
    );
    assert!(
        !out.contains("p  paste code"),
        "the row became the field:\n{out}"
    );
    assert!(!out.contains("PASTE CODE"), "no second modal:\n{out}");
    assert_eq!(footer_of(&out), footer);

    session_field(&mut app, Some(InputState::new("CANARYCODE#CANARYSTATE")));
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| super::draw(f, &app)).unwrap();
    let out: String = crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .map(|r| r + "\n")
        .collect();
    let field = row_of(&out, "✎ code CANARYCODE#CANARYSTATE");
    assert!(
        !out.contains("(paste it here)") && !out.contains("•") && !out.contains("chars)"),
        "the typed text, not a placeholder, a mask or a count:\n{out}"
    );
    let caret = term.get_cursor_position().unwrap();
    let field_line = out.lines().nth(field).unwrap_or("");
    let value_col = field_line
        .find("CANARYCODE")
        .map(|i| field_line[..i].chars().count())
        .unwrap_or(usize::MAX);
    assert_eq!(
        (usize::from(caret.x), usize::from(caret.y)),
        (value_col + "CANARYCODE#CANARYSTATE".len(), field),
        "the native caret sits after the typed text on the field row:\n{out}"
    );
    assert_eq!(footer_of(&out), footer);

    // A real `code#state` is a few hundred bytes and wraps inside an 80-column
    // modal: the caret folds onto the wrapped row, right after the tail.
    let long = format!("{}#TAIL", "X".repeat(90));
    session_field(&mut app, Some(InputState::new(&long)));
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| super::draw(f, &app)).unwrap();
    let out: String = crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .map(|r| r + "\n")
        .collect();
    let tail_row = out
        .lines()
        .position(|l| l.contains("#TAIL"))
        .expect("the wrapped tail renders");
    let head_row = row_of(&out, "✎ code X");
    assert!(
        tail_row > head_row,
        "the value wraps past its first row:\n{out}"
    );
    let tail_line = out.lines().nth(tail_row).unwrap_or("");
    let tail_end = tail_line
        .find("#TAIL")
        .map(|i| tail_line[..i].chars().count() + "#TAIL".len())
        .unwrap_or(usize::MAX);
    let caret = term.get_cursor_position().unwrap();
    assert_eq!(
        (usize::from(caret.x), usize::from(caret.y)),
        (tail_end, tail_row),
        "the native caret folds onto the wrapped row, after the tail:\n{out}"
    );

    session_stage(&mut app, LoginStage::ExchangingCode(LoginMethod::Browser));
    let out = dump(&app, 80, 24);
    for gone in ["✎ code", "CANARY", "pasting the code"] {
        assert!(
            !out.contains(gone),
            "`{gone}` never renders past the door:\n{out}"
        );
    }
    assert!(out.contains("exchanging the code for tokens"), "{out}");
}

fn session_field(app: &mut App, field: Option<crate::tui::app::InputState>) {
    if let Some(s) = app.login.as_mut() {
        s.paste_field = field;
    }
}

// ── the Fallback card's `preferred days` row ─────────────────────────────────

/// The Fallback detail, cursor on the `preferred days` row, ready to dump.
fn preferred_days_app(config: AppConfig) -> App {
    use crate::tui::app::{FALLBACK_ROWS, FallbackFocus, FallbackRow, Tab};
    let mut app = App::new(config);
    app.tab = Tab::Fallback;
    app.fallback_focus = FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::PreferredDays)
        .expect("the row exists");
    app
}

/// One chain member holding `days`, on the Fallback detail.
fn preferred_days_card(days: Vec<chrono::Weekday>) -> App {
    let mut p = oauth("uwuclxdy", 42.0, 18.0, true);
    p.preferred_days = days;
    preferred_days_app(AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: vec!["uwuclxdy".into()],
            fallback_chain: vec!["uwuclxdy".into()],
            ..AppState::default()
        },
        profiles: vec![p],
    })
}

/// Both hint arms, whole footer row: the row's own keys at rest, then the
/// picker's once ⏎ descends, where `←→` walk the chips (no `tabs` claim) and
/// `q` leaves (`q back`). The picker lets `?` through to the help modal, so
/// `? help` rides at every width, the doc floor included; its own groups stop
/// at three, `↵ done` left out since `q back` already names that exit. At
/// phone width the row sheds `↑↓ row` first, so `space toggle`, the one key
/// that picks, survives until the floor.
#[test]
fn fallback_preferred_days_footer_hints() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let mut app = preferred_days_card(Vec::new());

    let out = dump(&app, 120, 30);
    assert_eq!(
        footer_of(&out),
        "←→ tabs   space preset   ↵ days   ↑↓ row   a actions   ? help   q back",
        "the row's hints"
    );
    let out = dump(&app, 45, 30);
    assert_eq!(
        footer_of(&out),
        "←→ tabs   space preset   ? help   q back",
        "at phone width `a actions` and `↑↓ row` shed before the row's own keys"
    );

    crate::tui::app::handle_key(&mut app, key(KeyCode::Enter));
    let footers: Vec<String> = [120, 45, 28]
        .into_iter()
        .map(|w| footer_of(&dump(&app, w, 30)).to_string())
        .collect();
    assert_eq!(
        footers,
        [
            "←→ day   space toggle   ↑↓ row   ? help   q back",
            "←→ day   space toggle   ? help   q back",
            "←→ day   ? help   q back",
        ],
        "the picker's hints at 120, 45 and 28 columns"
    );
}

/// Every editor that owns the keyboard owns `←→` with it, so while one is open
/// the hint bar carries exactly one arrow group, naming that editor's own use
/// of the arrows; the same screen a key before, at rest, carries `←→ tabs`.
/// Whole footer rows, every editor checked before any assertion fires.
#[test]
fn an_editor_owning_the_arrows_names_them_and_drops_the_tab_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::testutil::key;
    use crate::tui::app::{FALLBACK_ROWS, FallbackRow, InputState, ServicesFocus, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;
    const TYPED: &str = "↵ save   ←→ caret   esc revert";

    let card = |row: FallbackRow| {
        let mut app = preferred_days_card(Vec::new());
        app.config().state.spend_budget_switching = true;
        app.fallback_detail_cursor = FALLBACK_ROWS
            .iter()
            .position(|r| *r == row)
            .expect("the row exists");
        app
    };
    let on_tab = |tab: Tab| {
        let mut app = preferred_days_card(Vec::new());
        app.tab = tab;
        app
    };
    let setup = || {
        let mut app = setup_app();
        handle_key(&mut app, key(KeyCode::Enter));
        app
    };
    let press_enter = |app: &mut App| handle_key(app, key(KeyCode::Enter));
    let mut plugin = on_tab(Tab::Services);
    plugin.services.focus = ServicesFocus::Detail;

    type Open = Box<dyn Fn(&mut App)>;
    let cases: Vec<(&str, App, Open, &str)> = vec![
        (
            "rotate at",
            card(FallbackRow::Threshold),
            Box::new(press_enter),
            TYPED,
        ),
        (
            "weekly at",
            card(FallbackRow::WeeklyAt),
            Box::new(press_enter),
            TYPED,
        ),
        (
            "max spend",
            card(FallbackRow::MaxSpend),
            Box::new(press_enter),
            TYPED,
        ),
        (
            "preferred days",
            card(FallbackRow::PreferredDays),
            Box::new(press_enter),
            "←→ day   space toggle   ↑↓ row   ? help   q back",
        ),
        (
            "refresh",
            on_tab(Tab::Config),
            Box::new(|app| app.refresh_interval_draft = Some(InputState::new("60"))),
            TYPED,
        ),
        (
            "context nudge",
            on_tab(Tab::Config),
            Box::new(|app| app.context_nudge_draft = Some(InputState::new("300k"))),
            TYPED,
        ),
        (
            "weekly limit",
            on_tab(Tab::Config),
            Box::new(|app| app.weekly_threshold_draft = Some(InputState::new("95"))),
            TYPED,
        ),
        (
            "herdr tag refresh",
            plugin,
            Box::new(|app| app.services.herdr_tag_draft = Some(InputState::new("5"))),
            TYPED,
        ),
        ("setup name", setup(), Box::new(press_enter), TYPED),
    ];

    let mut wrong = Vec::new();
    for (label, mut app, open, want) in cases {
        let rest = footer_of(&dump(&app, 120, 30)).to_string();
        if !rest.starts_with("←→ tabs") || rest.matches("←→").count() != 1 {
            wrong.push(format!("[{label}] at rest: {rest:?}"));
        }
        open(&mut app);
        let editing = footer_of(&dump(&app, 120, 30)).to_string();
        if editing != want {
            wrong.push(format!("[{label}] editing: {editing:?}, want {want:?}"));
        }
    }
    assert!(wrong.is_empty(), "footers:\n{}", wrong.join("\n"));
}

/// One account, `acct`, on the Setup tab's account list.
fn setup_app() -> App {
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("acct")],
            ..AppState::default()
        },
        profiles: vec![crate::testutil::blank_profile(&ProfileName::from("acct"))],
    });
    app.tab = crate::tui::app::Tab::Setup;
    app
}

/// A modal owns every key, so `←`/`→` never switch the tab under one and the
/// hint bar drops `←→ tabs` while it is open: the help sheet, a confirm (whose
/// `←→` flip its choice) and a name prompt (whose `←→` walk its caret). Every
/// modal checked before any assertion fires.
#[test]
fn a_modal_owns_the_arrows_so_the_footer_drops_the_tab_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::testutil::key;
    use crate::tui::app::{InputState, Modal, NamePromptAction, NamePromptForm, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;

    type Open = Box<dyn Fn(&mut App)>;
    let cases: Vec<(&str, Open)> = vec![
        (
            "help",
            Box::new(|app| handle_key(app, key(KeyCode::Char('?')))),
        ),
        (
            "rotate-all confirm",
            Box::new(|app| handle_key(app, key(KeyCode::Char('t')))),
        ),
        (
            "name prompt",
            Box::new(|app| {
                app.modals.push(Modal::NamePrompt(NamePromptForm {
                    input: InputState::new("ab"),
                    action: NamePromptAction::DuplicateProfile("acct".into()),
                }))
            }),
        ),
    ];
    let mut wrong = Vec::new();
    for (label, open) in cases {
        let mut app = setup_app();
        let rest = footer_of(&dump(&app, 120, 30)).to_string();
        if !rest.starts_with("←→ tabs") {
            wrong.push(format!("[{label}] at rest: {rest:?}"));
        }
        open(&mut app);
        if app.modals.is_empty() {
            wrong.push(format!("[{label}] opened no modal"));
            continue;
        }
        let under = footer_of(&dump(&app, 120, 30)).to_string();
        if under.contains("←→") {
            wrong.push(format!("[{label}] under the modal: {under:?}"));
        }
        handle_key(&mut app, key(KeyCode::Right));
        if app.tab != Tab::Setup {
            wrong.push(format!("[{label}] → switched the tab to {:?}", app.tab));
        }
    }
    assert!(wrong.is_empty(), "footers:\n{}", wrong.join("\n"));
}

/// With a login in flight and an editor open, esc goes to the editor, so the
/// login line's trailing hint is the editor's own row, whose `esc revert` names
/// the field rather than the login, and esc leaves the login running; with no
/// editor the same line offers the login's `esc cancel`, and esc cancels it.
#[test]
fn the_login_line_names_the_keys_an_open_editor_takes_first() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::testutil::key;
    use crate::tui::app::{InputState, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Config;
    let (session, _rx) = paste_session();
    app.login = Some(session);
    let spinner = super::format::spinner_frame(app.tick_count);

    app.refresh_interval_draft = Some(InputState::new("60"));
    assert_eq!(
        footer_of(&dump(&app, 100, 30)),
        format!("{spinner} logging in 'fresh'   ↵ save   ←→ caret   esc revert"),
        "the field's own keys"
    );
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(
        app.refresh_interval_draft.is_none() && app.login.is_some(),
        "esc closed the field and left the login running"
    );

    assert_eq!(
        footer_of(&dump(&app, 100, 30)),
        format!("{spinner} logging in 'fresh'   esc cancel"),
        "no editor: the login's own cancel"
    );
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(app.login.is_none(), "esc cancelled the login");
}

/// A narrow login line sheds an open editor's hints the way the plain footer
/// sheds its row, rightmost non-essential first, so the editor's exit hint
/// stays whole beside `logging in '<name>'` at 45 columns: a typed field sheds
/// its caret keys and keeps `↵ save`, the day picker keeps its `←→ day`.
#[test]
fn a_narrow_login_line_keeps_the_open_editors_exit_hint() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::testutil::key;
    use crate::tui::app::{InputState, Tab, handle_key};
    use ratatui::crossterm::event::KeyCode;

    let mut field = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    field.tab = Tab::Config;
    let (session, _field_rx) = paste_session();
    field.login = Some(session);
    field.refresh_interval_draft = Some(InputState::new("60"));
    let spinner = super::format::spinner_frame(field.tick_count);

    let mut picker = preferred_days_card(Vec::new());
    let (session, _picker_rx) = paste_session();
    picker.login = Some(session);
    handle_key(&mut picker, key(KeyCode::Enter));

    assert_eq!(
        [
            footer_of(&dump(&field, 45, 30)).to_string(),
            footer_of(&dump(&picker, 45, 30)).to_string(),
        ],
        [
            format!("{spinner} logging in 'fresh'   ↵ save   esc revert"),
            format!("{spinner} logging in 'fresh'   ←→ day   q back"),
        ],
        "[typed field, day picker] at 45"
    );
}

/// Esc keeps a typed value on the `+ new` form, so its field's footer reads
/// `esc done`, and ⏎ there only ends the field (the form saves whole from its
/// `create account` row), so it reads `↵ done` too; an existing account's
/// field saves on ⏎ and reverts on esc: `↵ save`, `esc revert`. Both rows
/// whole, and the kept value checked.
#[test]
fn the_new_account_form_labels_esc_done_and_keeps_the_typed_value() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::testutil::key;
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let mut app = setup_app();

    handle_key(&mut app, key(KeyCode::Enter));
    handle_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        footer_of(&dump(&app, 120, 30)),
        "↵ save   ←→ caret   esc revert",
        "an existing account's `name` field"
    );
    handle_key(&mut app, key(KeyCode::Esc));

    handle_key(&mut app, key(KeyCode::Char('n')));
    handle_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        footer_of(&dump(&app, 120, 30)),
        "↵ done   ←→ caret   esc done",
        "the `+ new` form's `name` field"
    );
    for c in ['a', 'b'] {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    handle_key(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.config_draft.as_ref().map(|d| d.name.value.as_str()),
        Some("ab"),
        "esc kept the typed name"
    );
}

/// A hand-written list keeps its own spelling: rendering the card and moving the
/// cursor across the row (`↓` then `↑` through `handle_key`) must not settle the
/// file into canonical order.
#[test]
fn a_hand_written_day_list_renders_without_rewriting_its_file() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::profile::ProfileName;
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let name = ProfileName::from("uwuclxdy");
    let path = crate::profile::profile_subpath(&name, "config.toml").expect("config path");

    // Seeded the way an operator would write it: the weekends the other way round.
    let mut seeded = oauth("uwuclxdy", 42.0, 18.0, true);
    seeded.preferred_days = vec![chrono::Weekday::Sun, chrono::Weekday::Sat];
    crate::profile::save_profile(&seeded).expect("seed the hand-written list");
    let before = std::fs::read(&path).expect("read the seeded file");
    assert!(
        String::from_utf8_lossy(&before).contains(r#"preferred_days = ["sun", "sat"]"#),
        "precondition: the file keeps the written order: {}",
        String::from_utf8_lossy(&before)
    );

    // Loaded back from that file, then rendered with the cursor on the row and
    // off it again.
    let loaded = crate::profile::load_profile(&name).expect("load the member");
    let mut app = preferred_days_app(AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: vec!["uwuclxdy".into()],
            fallback_chain: vec!["uwuclxdy".into()],
            ..AppState::default()
        },
        profiles: vec![loaded],
    });
    assert_eq!(
        app.config()
            .find(&name)
            .expect("the member")
            .preferred_days
            .clone(),
        vec![chrono::Weekday::Sun, chrono::Weekday::Sat],
        "precondition: the loader kept the written order"
    );

    let row = app.fallback_detail_cursor;
    let on_row = dump(&app, 120, 30);
    assert!(
        on_row.contains("preferred days  never  weekdays  [weekends]  every day"),
        "the unordered list reads as the `weekends` rung:\n{on_row}"
    );
    crate::tui::app::handle_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.fallback_detail_cursor, row + 1, "↓ left the row");
    let _ = dump(&app, 120, 30);
    crate::tui::app::handle_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.fallback_detail_cursor, row, "↑ came back to it");
    let _ = dump(&app, 120, 30);

    assert_eq!(
        std::fs::read(&path).expect("read back"),
        before,
        "rendering the card and moving across the row wrote nothing"
    );
}

/// The first card row carrying `key` and the `n - 1` lines under it, cut to the
/// detail pane's inner columns (the row opens `lead` cells before its key; the
/// pane's right padding and border are the frame's last two columns), right
/// edge trimmed.
fn detail_block(out: &str, key: &str, lead: usize, n: usize) -> Vec<String> {
    let rows: Vec<Vec<char>> = out.lines().map(|l| l.chars().collect()).collect();
    let key: Vec<char> = key.chars().collect();
    let (at, col) = rows
        .iter()
        .enumerate()
        .find_map(|(i, r)| r.windows(key.len()).position(|w| w == key).map(|c| (i, c)))
        .expect("the key's row renders");
    let (start, end) = (col - lead, rows[at].len() - 2);
    rows[at..at + n]
        .iter()
        .map(|r| {
            r[start..end]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// A picker row that overruns its pane breaks BETWEEN chips onto continuation
/// lines whose marks line up under the first line's, so every day is on screen
/// exactly once at the side-by-side widths that used to clip it (60-68) and at
/// the stacked phone width (45).
#[test]
fn the_day_picker_wraps_between_chips_at_every_width() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let two_a_line: &[&str] = &[
        "✎ preferred days ❯[ ]mon   [ ]tue",
        "                  [ ]wed   [ ]thu",
        "                  [ ]fri   [x]sat",
        "                  [x]sun",
    ];
    let three_a_line: &[&str] = &[
        "✎ preferred days ❯[ ]mon   [ ]tue   [ ]wed",
        "                  [ ]thu   [ ]fri   [x]sat",
        "                  [x]sun",
    ];
    // The last case is a pane only 12 rows tall: the card scrolls the picker's
    // continuation lines and its hint into view along with its first line.
    for (w, h, want) in [
        (60, 40, two_a_line),
        (64, 40, two_a_line),
        (68, 40, three_a_line),
        (45, 40, two_a_line),
        (60, 18, two_a_line),
    ] {
        let mut app = preferred_days_card(vec![chrono::Weekday::Sat, chrono::Weekday::Sun]);
        crate::tui::app::handle_key(&mut app, key(KeyCode::Enter));
        let block = detail_block(&dump(&app, w, h), "preferred days", 2, want.len() + 1);
        assert_eq!(block[..want.len()], want[..], "{w}x{h}");
        assert!(
            block[want.len()].starts_with(" └ "),
            "{w}x{h}: the picker's hint follows its last line, got {:?}",
            block[want.len()]
        );
        let chips = block[..want.len()].join("\n");
        for day in ["mon", "tue", "wed", "thu", "fri", "sat", "sun"] {
            assert_eq!(chips.matches(day).count(), 1, "{w}x{h}: `{day}` once");
        }
    }
}

/// A picker taller than the pane keeps the caret's own line on screen: seeded
/// on Monday the label line shows, and walked to Sunday the last line does.
#[test]
fn a_picker_taller_than_the_pane_keeps_its_caret_in_view() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    for (w, h) in [(35, 18), (30, 16), (28, 12)] {
        let mut app = preferred_days_card(vec![chrono::Weekday::Sat, chrono::Weekday::Sun]);
        crate::tui::app::handle_key(&mut app, key(KeyCode::Enter));
        let out = dump(&app, w, h);
        assert_eq!(
            out.lines()
                .filter(|l| l.contains("✎ preferred days ❯[ ]mon"))
                .count(),
            1,
            "{w}x{h}: the caret's first line is on screen:\n{out}"
        );
        crate::tui::app::handle_key(&mut app, key(KeyCode::Left));
        let out = dump(&app, w, h);
        assert_eq!(
            out.lines().filter(|l| l.contains("❯[x]sun")).count(),
            1,
            "{w}x{h}: the caret on Sunday is on screen:\n{out}"
        );
    }
}

/// The screen column where `key`'s value opens, on the first row carrying
/// `key` followed by the key gutter.
fn value_column(out: &str, key: &str) -> usize {
    let key: Vec<char> = key.chars().collect();
    out.lines()
        .find_map(|l| {
            let row: Vec<char> = l.chars().collect();
            let after = row.windows(key.len()).position(|w| w == key)? + key.len();
            let gap = row[after..].iter().take_while(|c| **c == ' ').count();
            (gap >= 2).then_some(after + gap)
        })
        .expect("the key renders with its value")
}

/// The Setup account card and the Fallback member card open their value
/// columns at the same screen column, so the values hold still across a tab
/// switch.
#[test]
fn the_setup_and_fallback_cards_open_their_values_on_one_column() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = preferred_days_card(Vec::new());
    let fallback = value_column(&dump(&app, 120, 30), "rotate at");
    app.tab = crate::tui::app::Tab::Setup;
    let setup = value_column(&dump(&app, 120, 30), "name");
    assert_eq!(setup, fallback);
}

/// The `5h usage` row gives its gauge only what the key column and the figure
/// leave, so the figure reads whole at the narrow side-by-side width (60) and
/// the stacked phone width (45) as well as on a desktop pane.
#[test]
fn the_five_hour_gauge_shrinks_so_its_figure_reads_whole() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = preferred_days_card(Vec::new());
    for (w, gauge) in [
        (45, "██████░░░░░░│░░"),
        (60, "████░░░░│░"),
        (120, "█████████░░░░░░░░░│░░░"),
    ] {
        assert_eq!(
            detail_block(&dump(&app, w, 40), "5h usage", 0, 2),
            vec![
                format!("5h usage        {gauge}  42% used"),
                "                38% until rotate".to_string(),
            ],
            "width {w}"
        );
    }
}

/// A custom day list at rest trails the preset run, whole on the run's line
/// when it fits and on a fresh line when it does not, and breaks between days,
/// never inside one, only where a line cannot hold it; every line past the
/// first indents to the value column, the open picker's break rule. Every day
/// is on screen once at every width a side-by-side or stacked pane takes.
#[test]
fn a_custom_day_list_at_rest_wraps_between_days() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let five = vec![Mon, Tue, Wed, Thu, Sat];
    let six = vec![Mon, Tue, Wed, Thu, Fri, Sat];
    let key = "❯ preferred days  ";
    let cont = " ".repeat(18);
    let cases: [(&Vec<chrono::Weekday>, u16, Vec<String>); 4] = [
        (
            &five,
            120,
            vec![format!(
                "{key}never  weekdays  weekends  every day  mon, tue, wed, thu, sat"
            )],
        ),
        (
            &six,
            120,
            vec![
                format!("{key}never  weekdays  weekends  every day"),
                format!("{cont}mon, tue, wed, thu, fri, sat"),
            ],
        ),
        (
            &six,
            60,
            vec![
                format!("{key}never  weekdays"),
                format!("{cont}weekends"),
                format!("{cont}every day"),
                format!("{cont}mon, tue, wed,"),
                format!("{cont}thu, fri, sat"),
            ],
        ),
        (
            &six,
            45,
            vec![
                format!("{key}never  weekdays"),
                format!("{cont}weekends  every day"),
                format!("{cont}mon, tue, wed, thu,"),
                format!("{cont}fri, sat"),
            ],
        ),
    ];
    for (days, w, want) in cases {
        let app = preferred_days_card(days.clone());
        let block = detail_block(&dump(&app, w, 40), "preferred days", 2, want.len() + 1);
        assert_eq!(block[..want.len()], want[..], "{} days at {w}", days.len());
        assert!(
            block[want.len()].starts_with(" └ "),
            "{} days at {w}: the row's hint follows its last line, got {:?}",
            days.len(),
            block[want.len()]
        );
        let row = block[..want.len()].join("\n");
        for day in ["mon", "tue", "wed", "thu", "sat"] {
            assert_eq!(
                row.matches(day).count(),
                1,
                "{} days at {w}: `{day}` once",
                days.len()
            );
        }
    }
}

/// The `preferred days` row at rest is the house cycle row: the whole preset
/// run, the held rung bracketed only while the row holds the cursor, a custom
/// list trailing the run. Pinned whole at a desktop width and at the 28-column
/// floor, where the value column cannot hold the widest chip, so the run drops
/// under its key instead of clipping a preset. At 31 and 32 columns the bare
/// `every day` fits the value column but its bracketed form does not: the
/// widest chip is measured with its brackets, so both rows stack alike,
/// focused or not, and `[every day]` reads whole. The line after each pin
/// shows the row ends where the pin does: its hint under a focused row, the
/// next row under a blurred one.
#[test]
fn the_preferred_days_row_at_rest_is_the_preset_run() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::FallbackFocus;
    use chrono::Weekday::*;
    let weekends = vec![Sat, Sun];
    let custom = vec![Mon, Wed, Fri];
    let every_day = crate::tui::app::WEEKDAYS_ALL.to_vec();
    let every_day_focused: &[&str] = &[
        "❯ preferred days",
        "  never  weekdays  weekends",
        "  [every day]",
    ];
    let every_day_blurred: &[&str] = &[
        "  preferred days",
        "  never  weekdays  weekends",
        "  every day",
    ];
    let cases: [(&Vec<chrono::Weekday>, u16, bool, &[&str]); 12] = [
        (&every_day, 31, true, every_day_focused),
        (&every_day, 31, false, every_day_blurred),
        (&every_day, 32, true, every_day_focused),
        (&every_day, 32, false, every_day_blurred),
        (
            &weekends,
            120,
            true,
            &["❯ preferred days  never  weekdays  [weekends]  every day"],
        ),
        (
            &weekends,
            120,
            false,
            &["  preferred days  never  weekdays  weekends  every day"],
        ),
        (
            &custom,
            120,
            true,
            &["❯ preferred days  never  weekdays  weekends  every day  mon, wed, fri"],
        ),
        (
            &custom,
            120,
            false,
            &["  preferred days  never  weekdays  weekends  every day  mon, wed, fri"],
        ),
        (
            &weekends,
            28,
            true,
            &[
                "❯ preferred days",
                "  never  weekdays",
                "  [weekends]  every day",
            ],
        ),
        (
            &weekends,
            28,
            false,
            &[
                "  preferred days",
                "  never  weekdays",
                "  weekends  every day",
            ],
        ),
        (
            &custom,
            28,
            true,
            &[
                "❯ preferred days",
                "  never  weekdays",
                "  weekends  every day",
                "  mon, wed, fri",
            ],
        ),
        (
            &custom,
            28,
            false,
            &[
                "  preferred days",
                "  never  weekdays",
                "  weekends  every day",
                "  mon, wed, fri",
            ],
        ),
    ];
    let mut wrong = Vec::new();
    for (days, w, focused, want) in cases {
        let mut app = preferred_days_card(days.clone());
        if !focused {
            app.fallback_focus = FallbackFocus::Chain;
        }
        let block = detail_block(&dump(&app, w, 40), "preferred days", 2, want.len() + 1);
        let next = if focused { " └ " } else { "  max spend" };
        if block[..want.len()] != want[..] || !block[want.len()].starts_with(next) {
            wrong.push(format!(
                "{days:?} at {w}, focused {focused}:\n{}",
                block.join("\n")
            ));
        }
    }
    assert!(wrong.is_empty(), "rows:\n{}", wrong.join("\n\n"));

    // At the floor every preset reads whole, whichever one the row holds.
    for (days, held) in [
        (Vec::new(), "[never]"),
        (vec![Mon, Tue, Wed, Thu, Fri], "[weekdays]"),
        (weekends.clone(), "[weekends]"),
        (crate::tui::app::WEEKDAYS_ALL.to_vec(), "[every day]"),
    ] {
        let app = preferred_days_card(days);
        let block = detail_block(&dump(&app, 28, 40), "preferred days", 2, 3).join("\n");
        for chip in [held, "never", "weekdays", "weekends", "every day"] {
            assert_eq!(
                block.matches(chip).count(),
                1,
                "`{chip}` reads whole once while the row holds {held}:\n{block}"
            );
        }
    }
}

/// Stepping past a custom list keeps it on the row while the card stays open:
/// the cycle comes back to it, so it stays in view after the run, in the
/// unselected options' TEXT_FAINT, while the rung it stepped onto takes the
/// ACCENT, `never` included.
#[test]
fn a_stepped_past_custom_list_stays_on_the_row_as_a_faint_stop() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::testutil::key;
    use chrono::Weekday::{Fri, Wed};
    use ratatui::crossterm::event::KeyCode;
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = preferred_days_card(vec![Wed, Fri]);
    // The row's save reads the member off disk and confirms its roster entry.
    let member = app.config().profiles[0].clone();
    crate::profile::save_profile(&member).expect("save the member");
    crate::testutil::register_names(&["uwuclxdy"]);
    crate::tui::app::handle_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        app.toasts.is_empty(),
        "precondition: the step saved, got {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );

    let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
    term.draw(|f| super::draw(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let rows = crate::testutil::buffer_rows(&buf);
    let out: String = rows.iter().map(|r| format!("{r}\n")).collect();
    assert_eq!(
        detail_block(&out, "preferred days", 2, 1),
        ["❯ preferred days  [never]  weekdays  weekends  every day  wed, fri"],
        "the stepped-past list stays on the row"
    );
    let y = rows
        .iter()
        .position(|r| r.contains("preferred days"))
        .expect("the row");
    let row: Vec<char> = rows[y].chars().collect();
    let x_of = |needle: &str| -> u16 {
        let needle: Vec<char> = needle.chars().collect();
        let x = row
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("the chip renders");
        u16::try_from(x).expect("fits the buffer")
    };
    let y = u16::try_from(y).expect("fits the buffer");
    let fg = |x: u16| buf[(x, y)].fg;
    assert_eq!(
        fg(x_of("never")),
        crate::tui::theme::accent_color(),
        "the held `never` is ACCENT"
    );
    for stop in ["wed,", "fri"] {
        assert_eq!(
            fg(x_of(stop)),
            crate::tui::theme::text_faint_color(),
            "the stepped-past `{stop}` is TEXT_FAINT"
        );
    }
}

/// The rows of the bordered box whose top border carries `title`, cut to the
/// box's inner columns (inside the border and its 1-cell padding), and each
/// row's right padding cell, where the overflow scrollbar lives.
fn boxed_rows(out: &str, title: &str) -> (Vec<String>, Vec<char>) {
    let rows: Vec<Vec<char>> = out.lines().map(|l| l.chars().collect()).collect();
    let (top, left) = rows
        .iter()
        .enumerate()
        .find_map(|(y, r)| {
            let text: String = r.iter().collect();
            let at = text[..text.find(title)?].chars().count();
            let x = r[..at].iter().rposition(|c| *c == '╭')?;
            Some((y, x))
        })
        .unwrap_or_else(|| panic!("the `{title}` box renders:\n{out}"));
    let right = rows[top][left..]
        .iter()
        .position(|c| *c == '╮')
        .map(|i| left + i)
        .expect("the box's top-right corner");
    let bottom = (top + 1..rows.len())
        .find(|y| rows[*y][left] == '╰')
        .expect("the box's bottom border");
    let inner = rows[top + 1..bottom]
        .iter()
        .map(|r| {
            r[left + 2..right - 1]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    let pad = rows[top + 1..bottom].iter().map(|r| r[right - 1]).collect();
    (inner, pad)
}

/// The `+ add` picker's view follows its cursor: with more candidates than the
/// pane holds, the one under the cursor and the day list it carries stay on
/// screen, and the overflow draws the scrollbar in the pane's right padding,
/// its thumb at the bottom of the track.
#[test]
fn the_add_picker_follows_its_cursor_and_draws_its_scrollbar() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::{FallbackFocus, Tab};
    let mut profiles = vec![oauth("a", 42.0, 18.0, false)];
    for i in 1..=12 {
        profiles.push(oauth(&format!("c{i:02}"), 10.0, 10.0, false));
    }
    profiles[12].preferred_days = vec![chrono::Weekday::Sat, chrono::Weekday::Sun];
    let names = profiles.iter().map(|p| p.name.clone()).collect();
    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: names,
            fallback_chain: vec!["a".into()],
            ..AppState::default()
        },
        profiles,
    });
    app.tab = Tab::Fallback;
    app.chain_cursor = 1;
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_detail_cursor = 11;

    let out = dump(&app, 80, 20);
    let (inner, pad) = boxed_rows(&out, "ADD TO CHAIN");
    assert_eq!(
        inner[inner.len() - 2..],
        ["❯ c12", " └ brings back its preferred days: weekends"],
        "the cursor's candidate and its note close the view:\n{out}"
    );
    assert!(
        pad.iter().all(|c| matches!(c, '┊' | '┃'))
            && pad.contains(&'┊')
            && pad.last() == Some(&'┃'),
        "the scrollbar fills the padding column, thumb at the bottom: {pad:?}\n{out}"
    );
}

/// The member card keeps its own scroll rule (the cursored row and its hint in
/// view) and shows its overflow like every other form pane: the scrollbar in
/// the right padding column.
#[test]
fn an_overflowing_member_card_draws_its_scrollbar() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = preferred_days_card(Vec::new());
    app.fallback_detail_cursor = crate::tui::app::FALLBACK_ROWS.len() - 1;

    let out = dump(&app, 45, 18);
    let (inner, pad) = boxed_rows(&out, "uwuclxdy ");
    assert!(
        inner.iter().any(|r| r.starts_with("❯ remove")),
        "the cursored row is in view:\n{out}"
    );
    assert!(
        pad.iter().all(|c| matches!(c, '┊' | '┃')) && pad.contains(&'┃'),
        "the card's overflow draws the scrollbar: {pad:?}\n{out}"
    );
}

/// The `└` hint that opens after the rows in `inner`, its wrapped lines joined
/// back into the one sentence it renders; `None` when no hint is on screen.
fn hint_on_screen(inner: &[String]) -> Option<String> {
    let mut hint = inner
        .iter()
        .skip_while(|r| !r.starts_with(" └ "))
        .take_while(|r| r.starts_with(" └ ") || r.starts_with("   "));
    let first = hint.next()?.strip_prefix(" └ ")?.to_string();
    Some(hint.fold(first, |acc, r| format!("{acc} {}", r.trim_start())))
}

/// Every at-rest arm of the `preferred days` hint ends on `↵ picks days one by
/// one`, the one place a narrow pane names the picker's key, so the card
/// scrolls the row's whole hint into view however far the pane wraps it: at
/// the 28-column floor and at phone width, on an ordinary 24-row terminal,
/// each arm reads whole, tail included.
#[test]
fn the_preferred_days_hint_reads_whole_on_a_narrow_card() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::{Fri, Sat, Sun};
    let member = |name: &str, days: Vec<chrono::Weekday>, disabled: bool| {
        let mut p = oauth(name, 42.0, 18.0, false);
        p.preferred_days = days;
        p.disabled = disabled;
        p
    };
    let card = |profiles: Vec<Profile>, active: Option<&str>| {
        let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
        preferred_days_app(AppConfig {
            state: AppState {
                active_profile: active.map(Into::into),
                profiles: names.clone(),
                fallback_chain: names,
                ..AppState::default()
            },
            profiles,
        })
    };
    const TAIL: &str = " · ↵ picks days one by one";
    let cases = [
        (
            "no list",
            card(
                vec![member("uwuclxdy", Vec::new(), false)],
                Some("uwuclxdy"),
            ),
            format!("work returns to this account on the days set here{TAIL}"),
        ),
        (
            "a day another list shares",
            card(
                vec![
                    member("uwuclxdy", vec![Sat, Sun], false),
                    member("b", vec![Fri, Sat], false),
                ],
                Some("uwuclxdy"),
            ),
            format!(
                "another list also names sat: work returns to whichever account reads clear \
                 first{TAIL}"
            ),
        ),
        (
            "a disabled account",
            card(vec![member("uwuclxdy", vec![Sat], true)], None),
            format!("a day list here would claim nothing: the account is disabled{TAIL}"),
        ),
    ];
    let mut wrong = Vec::new();
    for (arm, app, want) in &cases {
        for (w, h) in [(28, 24), (45, 24)] {
            let out = dump(app, w, h);
            let (inner, _) = boxed_rows(&out, "uwuclxdy ");
            let got = hint_on_screen(&inner);
            if got.as_deref() != Some(want.as_str()) {
                wrong.push(format!("[{arm}] at {w}x{h}: {got:?}\n{out}"));
            }
        }
    }
    assert!(wrong.is_empty(), "hints:\n{}", wrong.join("\n"));
}

/// A candidate mid-list keeps its whole note on screen, not only its own line:
/// the view holds the note as part of the cursor's block. At 28 columns the
/// blocker note wraps to four lines, past the three rows of context the scroll
/// keeps after a line, with more candidates below to scroll into.
#[test]
fn a_mid_list_add_candidate_keeps_its_whole_note_on_screen() {
    let _home = crate::testutil::HomeSandbox::new();
    use crate::tui::app::{FallbackFocus, Tab};
    let mut profiles = vec![oauth("a", 42.0, 18.0, false)];
    for i in 1..=12 {
        profiles.push(oauth(&format!("c{i:02}"), 10.0, 10.0, false));
    }
    profiles[8].preferred_days = vec![chrono::Weekday::Sat, chrono::Weekday::Sun];
    let names = profiles.iter().map(|p| p.name.clone()).collect();
    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: names,
            fallback_chain: vec!["a".into()],
            auth_broken: vec!["c08".into()],
            ..AppState::default()
        },
        profiles,
    });
    app.tab = Tab::Fallback;
    app.chain_cursor = 1;
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_detail_cursor = 7;

    let out = dump(&app, 28, 24);
    let (inner, _) = boxed_rows(&out, "ADD TO CHAIN");
    let at = inner
        .iter()
        .position(|r| r == "❯ c08")
        .unwrap_or_else(|| panic!("the cursor's candidate is on screen:\n{out}"));
    assert_eq!(
        hint_on_screen(&inner[at..]).as_deref(),
        Some("its day list (weekends) would claim nothing: its login is auth-broken"),
        "the whole note is on screen:\n{out}"
    );
    let note_lines = inner[at + 1..]
        .iter()
        .take_while(|r| r.starts_with(" └ ") || r.starts_with("   "))
        .count();
    assert!(
        note_lines > 3,
        "precondition: the note wraps past the scroll's 3-row pad:\n{out}"
    );
    assert!(
        inner[at + 1 + note_lines..]
            .iter()
            .any(|r| r.starts_with("  c")),
        "precondition: candidates follow the note, so the content end does not pin it:\n{out}"
    );
}
