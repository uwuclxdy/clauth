use crate::lockorder::RankedMutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::usage::{ActivityStore, ProfileActivity, any_busy};

fn make_activity(entries: &[(&str, ProfileActivity)]) -> ActivityStore {
    let mut map = HashMap::new();
    for (name, activity) in entries {
        map.insert(name.to_string(), *activity);
    }
    Arc::new(RankedMutex::new(map))
}

fn bootstrap_busy(flag: &Arc<AtomicBool>, activity: &ActivityStore) -> bool {
    flag.load(Ordering::SeqCst) || any_busy(activity)
}

use super::{InputState, parse_threshold};

#[test]
fn delete_word_removes_run_left_of_caret() {
    let mut input = InputState::new("foo bar");
    input.delete_word();
    assert_eq!(input.value, "foo ");
    input.delete_word();
    assert_eq!(input.value, "");
}

#[test]
fn delete_word_respects_caret_position() {
    let mut input = InputState::new("foo bar");
    input.home();
    input.delete_word();
    assert_eq!(input.value, "foo bar");
}

#[test]
fn parse_threshold_accepts_in_range_only() {
    assert_eq!(parse_threshold("0"), Some(0.0));
    assert_eq!(parse_threshold("100"), Some(100.0));
    assert_eq!(parse_threshold("73.5"), Some(73.5));
    assert!(parse_threshold("150").is_none());
    assert!(parse_threshold("-1").is_none());
    assert!(parse_threshold("abc").is_none());
    assert!(parse_threshold("").is_none());
}

#[test]
fn bootstrap_active_true_reports_busy() {
    let flag = Arc::new(AtomicBool::new(true));
    let activity = make_activity(&[]);
    assert!(bootstrap_busy(&flag, &activity));
}

#[test]
fn bootstrap_active_false_empty_store_reports_idle() {
    let flag = Arc::new(AtomicBool::new(false));
    let activity = make_activity(&[]);
    assert!(!bootstrap_busy(&flag, &activity));
}

#[test]
fn bootstrap_active_true_with_refreshing_slot_reports_busy() {
    let flag = Arc::new(AtomicBool::new(true));
    let activity = make_activity(&[("alice", ProfileActivity::Refreshing)]);
    assert!(bootstrap_busy(&flag, &activity));
}

#[test]
fn bootstrap_active_false_with_refreshing_slot_still_busy() {
    let flag = Arc::new(AtomicBool::new(false));
    let activity = make_activity(&[("alice", ProfileActivity::Refreshing)]);
    assert!(bootstrap_busy(&flag, &activity));
}

// ── compact mode ─────────────────────────────────────────────────────────

use super::App;

fn bare_app() -> App {
    use crate::profile::{AppConfig, AppState};
    App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    })
}

/// Seed CC's plugin registry with one clauth install record at `scope`.
fn write_plugin_install(scope: &str) {
    let path = crate::profile::claude_dir()
        .expect("claude dir")
        .join("plugins")
        .join("installed_plugins.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let body = serde_json::json!({
        "plugins": { "clauth@clauth": [{ "scope": scope, "version": "0.1.0" }] }
    });
    std::fs::write(&path, serde_json::to_vec(&body).expect("serialize")).expect("write");
}

fn plugin_check(app: &App) -> &super::Check {
    app.plugin
        .checks
        .iter()
        .find(|c| c.label == "plugin")
        .expect("plugin check present")
}

#[test]
fn plugin_check_ok_when_installed_globally() {
    let _home = crate::testutil::HomeSandbox::new();
    write_plugin_install("user");
    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);
    let check = plugin_check(&app);
    assert_eq!(check.health, super::Health::Ok);
    assert!(
        check
            .detail
            .iter()
            .any(|line| line.starts_with("installed: yes")),
        "global install should read installed, got {:?}",
        check.detail
    );
}

#[test]
fn plugin_check_warns_and_suggests_global_when_project_local() {
    let _home = crate::testutil::HomeSandbox::new();
    write_plugin_install("local");
    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);
    let check = plugin_check(&app);
    assert_eq!(check.health, super::Health::Warn);
    assert!(
        check.detail.iter().any(|line| line.contains("(local)")),
        "the project-local scope should surface in the readout, got {:?}",
        check.detail
    );
    assert!(
        check
            .detail
            .iter()
            .any(|line| line.contains("--scope user")),
        "non-global install should suggest a user-scope install, got {:?}",
        check.detail
    );
}

fn mcp_check(app: &App) -> &super::Check {
    app.plugin
        .checks
        .iter()
        .find(|c| c.label == "mcp servers")
        .expect("mcp servers check present")
}

#[test]
fn mcp_check_ok_when_globally_wired() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::plugin_probe::wire_mcp_server().expect("wire ~/.claude.json");
    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);
    let check = mcp_check(&app);
    assert_eq!(check.health, super::Health::Ok);
    assert!(
        check.detail.iter().any(|line| line == "present: yes"),
        "a globally wired server should read present, got {:?}",
        check.detail
    );
    assert!(check.fix.is_none());
}

#[test]
fn mcp_check_warns_project_only_for_local_plugin() {
    let _home = crate::testutil::HomeSandbox::new();
    // A project-scope plugin advertises the server for one repo only, and no
    // global `~/.claude.json` entry exists in the sandbox to make it global.
    write_plugin_install("local");
    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);
    let check = mcp_check(&app);
    assert_eq!(check.health, super::Health::Warn);
    assert!(
        check
            .detail
            .iter()
            .any(|line| line == "wired for this project only, not global"),
        "project-only wiring should say so in the readout, got {:?}",
        check.detail
    );
    assert!(check.fix.is_some(), "should offer the global write fix");
}

#[test]
fn runtime_check_summarizes_profiles() {
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![Profile::new("acct".to_string(), None, None)],
    });
    super::recompute_plugin_checks(&mut app, false);

    let check = app
        .plugin
        .checks
        .iter()
        .find(|c| c.label == "runtime")
        .expect("runtime check");
    // One idle, non-active, credential-less profile: no active link, no live
    // sessions → a neutral dot (not green) and no fix.
    assert_eq!(check.health, super::Health::Idle);
    assert!(check.fix.is_none());
    assert!(check.detail.iter().any(|l| l == "profiles: 1"));
    assert!(check.detail.iter().any(|l| l == "sessions: 0"));
    assert!(check.detail.iter().any(|l| l == "active: \u{2014}"));
    assert!(check.detail.iter().any(|l| l == "link: \u{2014}"));
}

#[test]
fn config_rows_login_and_delete_creds_visibility() {
    use super::{ConfigRow, config_rows};
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let creds = || ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };

    let mut oauth_with = Profile::new("oauth-with".to_string(), None, None);
    oauth_with.credentials = Some(creds());
    let oauth_without = Profile::new("oauth-without".to_string(), None, None);
    let api_no_key = Profile::new(
        "api-no-key".to_string(),
        Some("https://api.example.com".to_string()),
        None,
    );
    let api_with_key = Profile::new(
        "api-with-key".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-secret".to_string()),
    );

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_with, oauth_without, api_no_key, api_with_key],
    });
    app.config_draft = None;

    // OAuth account holding creds → re-login row plus delete-creds row.
    app.profile_cursor = 0;
    let rows = config_rows(&app);
    assert!(rows.contains(&ConfigRow::Login), "oauth+creds shows login");
    assert!(
        rows.contains(&ConfigRow::DeleteCreds),
        "oauth+creds shows delete-creds"
    );

    // OAuth shell with no creds → login only.
    app.profile_cursor = 1;
    let rows = config_rows(&app);
    assert!(rows.contains(&ConfigRow::Login), "oauth blank shows login");
    assert!(
        !rows.contains(&ConfigRow::DeleteCreds),
        "oauth blank hides delete-creds"
    );

    // API account with no key → login (re-enter url+key), no log-out yet.
    app.profile_cursor = 2;
    let rows = config_rows(&app);
    assert!(rows.contains(&ConfigRow::Login), "api blank shows login");
    assert!(
        !rows.contains(&ConfigRow::DeleteCreds),
        "api blank hides delete-creds"
    );

    // API account holding a key → login (re-login) plus log-out.
    app.profile_cursor = 3;
    let rows = config_rows(&app);
    assert!(rows.contains(&ConfigRow::Login), "api+key shows login");
    assert!(
        rows.contains(&ConfigRow::DeleteCreds),
        "api+key shows delete-creds"
    );

    // `+ new` form with an empty base_url buffer → login before create.
    app.profile_cursor = 4;
    let rows = config_rows(&app);
    let login_idx = rows
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("new form shows login");
    let create_idx = rows
        .iter()
        .position(|r| *r == ConfigRow::Create)
        .expect("new form shows create");
    assert!(
        login_idx < create_idx,
        "login precedes create on the new form"
    );
}

/// `ConfigRow` derives no `Ord`/`EnumIter`, so nothing but this render order is
/// observable — pin auto-start's second-slot head plus the account-actions
/// tail's exact RUNTIME sequence (`config_rows`'s own row order) so a future
/// reorder there reds instead of silently drifting from the enum's declaration
/// order.
#[test]
fn config_rows_account_actions_tail_matches_runtime_order() {
    use super::{ConfigRow, config_rows};
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("acct".to_string(), None, None);
    acct.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![acct],
    });
    app.profile_cursor = 0;
    app.config_draft = None;

    let rows = config_rows(&app);
    // Full runtime sequence for this fixture (OAuth account, no base url, no
    // overrides, no custom env, holding OAuth credentials): auto-start in the
    // second slot, the alias overrides collapsed behind `ModelOverrideAdd`, no
    // env rows, then the login/delete-creds/disabled/delete action tail. A
    // future reorder of `config_rows`' row-construction (the `rows.push(...)`
    // builder) reds here; a match-arm reorder elsewhere is unobservable at
    // runtime and isn't what this test guards.
    assert_eq!(
        rows,
        [
            ConfigRow::Name,
            ConfigRow::AutoStart,
            ConfigRow::BaseUrl,
            ConfigRow::Model,
            ConfigRow::ModelOverrideAdd,
            ConfigRow::EnvAdd,
            ConfigRow::Login,
            ConfigRow::DeleteCreds,
            ConfigRow::Disabled,
            ConfigRow::Delete,
        ],
        "config_rows must render this exact sequence for an OAuth account with \
         credentials, no overrides, and no custom env: {rows:?}"
    );
}

/// The API-account re-login row walks a two-step inline chain: base url first,
/// then api key, persisting both like `login --base-url --api-key`. ⎋ at either
/// step abandons the whole chain.
#[test]
fn api_relogin_chain_walks_base_url_then_api_key() {
    use super::{
        ConfigFocus, ConfigRow, InputState, cancel_config_edit, commit_config_field, config_rows,
        enter_config_detail, run_config_row,
    };
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let api = Profile::new(
        "api".to_string(),
        Some("https://old.example.com".to_string()),
        Some("old-key".to_string()),
    );

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec!["api".into()],
            ..AppState::default()
        },
        profiles: vec![api],
    });
    app.profile_cursor = 0;
    enter_config_detail(&mut app);
    assert_eq!(app.config_focus, ConfigFocus::Actions);

    // Activate the re-login row → chain opens on the base-url field.
    let rows = config_rows(&app);
    app.config_action_cursor = rows
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("api account shows a login row");
    run_config_row(&mut app, ConfigRow::Login);
    {
        let d = app.config_draft.as_ref().expect("draft");
        assert!(d.relogin_chain, "re-login opens the chain");
        assert_eq!(
            d.active,
            Some(ConfigRow::BaseUrl),
            "chain starts on base url"
        );
    }

    // Type a fresh base url and commit → advances to the api-key step.
    app.config_draft.as_mut().unwrap().base_url = InputState::new("https://new.example.com");
    commit_config_field(&mut app, ConfigRow::BaseUrl);
    {
        let d = app.config_draft.as_ref().expect("draft");
        assert!(d.relogin_chain, "chain still live after the base-url step");
        assert_eq!(
            d.active,
            Some(ConfigRow::ApiKey),
            "chain advances to api key"
        );
    }

    // Type a fresh key and commit → chain ends, both values persisted.
    app.config_draft.as_mut().unwrap().api_key = InputState::new("new-key");
    commit_config_field(&mut app, ConfigRow::ApiKey);
    {
        let d = app.config_draft.as_ref().expect("draft");
        assert!(!d.relogin_chain, "chain cleared after the api-key step");
        assert_eq!(d.active, None, "editing ended");
    }
    {
        let cfg = app.config();
        let p = cfg.find("api").expect("profile present");
        assert_eq!(p.base_url.as_deref(), Some("https://new.example.com"));
        assert_eq!(p.api_key.as_deref(), Some("new-key"));
    }

    // ⎋ mid-chain abandons it: re-open, then cancel on the base-url step.
    run_config_row(&mut app, ConfigRow::Login);
    assert!(app.config_draft.as_ref().unwrap().relogin_chain);
    cancel_config_edit(&mut app, ConfigRow::BaseUrl);
    let d = app.config_draft.as_ref().expect("draft");
    assert!(!d.relogin_chain, "⎋ abandons the chain");
    assert_eq!(d.active, None, "⎋ ends editing");
}

#[test]
fn config_rows_login_tracks_api_mode_when_draft_types_a_base_url() {
    use super::{ConfigRow, InputState, build_draft_existing, build_draft_new, config_rows};
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let mut oauth = Profile::new("oauth".to_string(), None, None);
    oauth.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![oauth],
    });

    // Existing OAuth draft that types a base url flips the endpoint rows to API
    // mode (api key shows), but the login rows type off the stored credential:
    // committing that base url would make this a hybrid, and its OAuth pair stays
    // logged out-able throughout.
    app.profile_cursor = 0;
    let mut draft = build_draft_existing(&app, "oauth");
    draft.base_url = InputState::new("https://api.example.com");
    app.config_draft = Some(draft);
    let rows = config_rows(&app);
    assert!(
        rows.contains(&ConfigRow::Login),
        "typing a base url keeps login (re-login) on an existing account"
    );
    assert!(
        rows.contains(&ConfigRow::ApiKey),
        "typing a base url reveals the api key row"
    );
    assert!(
        rows.contains(&ConfigRow::DeleteCreds),
        "an uncommitted base url can't hide the stored OAuth pair's log-out row"
    );

    // `+ new` form with a typed base url is an API create → no login row (the
    // base url + api key + create rows already stand in for it).
    app.profile_cursor = 1;
    let mut draft = build_draft_new();
    draft.base_url = InputState::new("https://api.example.com");
    app.config_draft = Some(draft);
    let rows = config_rows(&app);
    assert!(
        !rows.contains(&ConfigRow::Login),
        "the new form hides login once a base url makes it an API account"
    );
}

/// A hybrid account: a stored OAuth pair AND a base url on one profile. Capture
/// reads the two live files independently, and setting a base url on an OAuth
/// account never drops its credentials — so this shape is reachable from both
/// paths, and the Setup rows must act on the credential that actually exists.
fn hybrid(name: &str, api_key: Option<&str>) -> crate::profile::Profile {
    let mut p = crate::profile::Profile::new(
        name.to_string(),
        Some("https://api.example.com".to_string()),
        api_key.map(str::to_string),
    );
    p.credentials = Some(login_creds("ref"));
    p
}

fn app_with(profiles: Vec<crate::profile::Profile>) -> App {
    use crate::profile::{AppConfig, AppState};
    let names = profiles.iter().map(|p| p.name.clone()).collect();
    App::new(AppConfig {
        state: AppState {
            profiles: names,
            ..AppState::default()
        },
        profiles,
    })
}

// ── lock-order regression ───────────────────────────────────────────────────

/// `reload_if_state_changed` must NOT hold the config guard while it writes the
/// `usage_tokens` / `third_party_tokens` mutexes: those rank OUTSIDE `Config`
/// (`Tokens` 250, `ThirdParty` 260, both `< Config` 400), so nesting them under
/// config inverts the global lock order. In debug builds the ranked-mutex
/// assertion panics ("lock-order violation: acquiring rank 250 while holding
/// [400]") the instant the inverted acquire runs, so this test reds if the fix
/// is reverted to the nested shape. The `assert!(reloaded)` is load-bearing: it
/// proves the reload branch actually ran and reached the token-mutex writes, so
/// a green here is never vacuous.
#[test]
fn reload_if_state_changed_does_not_invert_config_over_token_locks() {
    use crate::profile::{AppState, Profile, save_app_state, save_profile};
    use crate::testutil::set_mtime;
    use std::time::{Duration, UNIX_EPOCH};

    let home = crate::testutil::HomeSandbox::new();

    // Persist a real profile so `load_config()` inside the reload succeeds.
    let profile = Profile::new("acct".to_string(), None, None);
    save_app_state(&AppState {
        profiles: vec![profile.name.clone()],
        ..AppState::default()
    })
    .expect("persist app state");
    save_profile(&profile).expect("persist profile config");

    let mut app = app_with(vec![profile]);

    // Force the fingerprint to differ from the one captured at construction so
    // `reload_if_state_changed` takes the reload branch instead of early-out.
    let config_toml = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("acct")
        .join("config.toml");
    set_mtime(&config_toml, UNIX_EPOCH + Duration::from_secs(1_000_000));

    let reloaded = app.reload_if_state_changed();
    assert!(
        reloaded,
        "the reload branch must run so the token-mutex writes are exercised"
    );
}

#[test]
fn config_rows_hybrid_shows_the_logout_row_for_its_oauth_pair() {
    use super::{ConfigRow, config_rows};
    let _home = crate::testutil::HomeSandbox::new();

    // No api key: the endpoint needs none (a local base url), so the only stored
    // credential is the OAuth pair.
    let mut app = app_with(vec![hybrid("hybrid", None)]);
    app.config_draft = None;
    app.profile_cursor = 0;

    let rows = config_rows(&app);
    assert!(
        rows.contains(&ConfigRow::DeleteCreds),
        "a stored OAuth pair keeps the log-out row on a hybrid: {rows:?}"
    );
}

#[test]
fn hybrid_logout_clears_the_oauth_pair_and_keeps_the_api_shell() {
    use super::{ConfirmAction, run_confirm_action};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![hybrid("hybrid", Some("sk-secret"))]);
    run_confirm_action(&mut app, ConfirmAction::BlankCredentials("hybrid".into()));

    let cfg = app.config();
    let p = cfg.find("hybrid").expect("profile present");
    assert!(
        p.credentials.is_none(),
        "log out drops the stored OAuth pair, not just the api key"
    );
    assert_eq!(
        p.base_url.as_deref(),
        Some("https://api.example.com"),
        "the endpoint shell survives the log out"
    );
    assert_eq!(
        p.api_key.as_deref(),
        Some("sk-secret"),
        "an OAuth log out leaves the api key alone"
    );
}

#[test]
fn hybrid_login_row_routes_to_the_browser_mint_not_the_api_chain() {
    use super::{ConfigRow, Modal, build_draft_existing, run_config_row};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![hybrid("hybrid", Some("sk-secret"))]);
    // A login already in flight parks `start_login` on its in-progress guard, so
    // the route is observable without minting anything.
    app.login_generation = 1;
    app.login = Some(login_session("other", true, 1));
    app.profile_cursor = 0;
    let draft = build_draft_existing(&app, "hybrid");
    app.config_draft = Some(draft);

    run_config_row(&mut app, ConfigRow::Login);
    assert!(
        app.modals.iter().any(|m| matches!(m, Modal::Login)),
        "a hybrid's login row runs the OAuth mint"
    );
    assert!(
        !app.config_draft.as_ref().is_some_and(|d| d.relogin_chain),
        "a hybrid's login row is not the API base-url + api-key re-entry"
    );
}

/// Pin: a pure API account (no stored OAuth pair) logs out of its api key only.
#[test]
fn pure_api_logout_clears_only_the_api_key() {
    use super::{ConfirmAction, run_confirm_action};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new(
        "api".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-secret".to_string()),
    )]);
    run_confirm_action(&mut app, ConfirmAction::BlankCredentials("api".into()));

    let cfg = app.config();
    let p = cfg.find("api").expect("profile present");
    assert_eq!(p.api_key, None, "log out blanks the api key");
    assert_eq!(
        p.base_url.as_deref(),
        Some("https://api.example.com"),
        "the endpoint shell survives the log out"
    );
}

/// Pin: a pure OAuth account logs out of its credentials, endpoint-less as ever.
#[test]
fn pure_oauth_logout_clears_the_credentials() {
    use super::{ConfirmAction, run_confirm_action};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut oauth = Profile::new("oauth".to_string(), None, None);
    oauth.credentials = Some(login_creds("ref"));
    let mut app = app_with(vec![oauth]);
    run_confirm_action(&mut app, ConfirmAction::BlankCredentials("oauth".into()));

    let cfg = app.config();
    let p = cfg.find("oauth").expect("profile present");
    assert!(
        p.credentials.is_none(),
        "log out drops the stored OAuth pair"
    );
    assert_eq!(p.base_url, None, "no endpoint appears out of a log out");
}

/// Simulate a live `clauth start` session for `name`: a locked pid file in its
/// sessions dir reads as alive via `has_live_session` (mirrors the fixture in
/// `tests/inline/actions.rs::delete_refuses_live_session_unless_forced`). The
/// caller must keep the returned file alive for as long as the session should
/// read as live — dropping it releases the flock.
fn arm_live_session(home: &std::path::Path, name: &str) -> std::fs::File {
    let sessions = home
        .join(".clauth")
        .join("profiles")
        .join(name)
        .join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("99999")).expect("open pid");
    pid.lock().expect("lock pid");
    pid
}

/// A live-session delete must not dead-end on the guard's refusal toast: it
/// arms a confirm modal instead, leaving the profile untouched until confirmed.
#[test]
fn perform_delete_with_live_session_arms_a_confirm_modal() {
    use super::{ConfirmAction, Modal, perform_delete, run_confirm_action};
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("busy".to_string(), None, None)]);
    let _pid_guard = arm_live_session(home.home(), "busy");

    perform_delete(&mut app, "busy");
    assert!(
        app.config().find("busy").is_some(),
        "a live-session delete must not remove the profile before confirmation"
    );
    let confirm = app
        .modals
        .last()
        .and_then(|m| match m {
            Modal::Confirm(s) => Some(s),
            _ => None,
        })
        .expect("a live session arms a confirm modal");
    assert!(
        matches!(&confirm.on_confirm, ConfirmAction::DeleteLiveSession(n) if n == "busy"),
        "the confirm carries the delete-live-session action for the right profile"
    );

    let action = confirm.on_confirm.clone();
    run_confirm_action(&mut app, action);
    assert!(
        app.config().find("busy").is_none(),
        "confirming deletes the profile despite the live session"
    );
}

/// No live session: the delete must land immediately, bit-identical to the
/// pre-existing behavior, with no confirm modal in the way.
#[test]
fn perform_delete_without_live_session_deletes_immediately() {
    use super::{Modal, perform_delete};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("quiet".to_string(), None, None)]);

    perform_delete(&mut app, "quiet");

    assert!(
        app.config().find("quiet").is_none(),
        "a delete with no live session removes the profile right away"
    );
    assert!(
        !app.modals.iter().any(|m| matches!(m, Modal::Confirm(_))),
        "no live session means no confirm modal is pushed"
    );
}

// ── `disabled` row (per-account disable toggle) ─────────────────────────────

/// Toggling `disabled` persists immediately into the live shared `AppConfig`
/// (`app.config()` IS what render reads next frame — no reload round-trip) and
/// to disk under the literal `disabled = true` key (`render_config_toml`), and
/// toggling again returns the account to full participation with no stale
/// state (the flag round-trips to exactly `false`, and every other field is
/// left untouched).
#[test]
fn toggle_profile_disabled_persists_to_memory_and_disk_and_round_trips() {
    use super::toggle_profile_disabled;
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut profile = Profile::new("acct".to_string(), None, None);
    profile.auto_start = true;
    let mut app = app_with(vec![profile]);

    toggle_profile_disabled(&mut app, "acct");
    assert!(
        app.config().find("acct").unwrap().is_disabled(),
        "the flag flips in the live shared config"
    );
    let on_disk = std::fs::read_to_string(
        home.home()
            .join(".clauth")
            .join("profiles")
            .join("acct")
            .join("config.toml"),
    )
    .expect("config.toml written");
    assert!(
        on_disk.contains("disabled = true"),
        "the literal on-disk key is written: {on_disk}"
    );

    toggle_profile_disabled(&mut app, "acct");
    let p = app.config().find("acct").cloned().unwrap();
    assert!(!p.is_disabled(), "toggling again re-enables it");
    assert!(
        p.auto_start,
        "no stale state: an unrelated field survives both toggles untouched"
    );
}

/// The gate mirrors `actions::disable_profile`'s own CLI-parity refusal
/// (`is_active` / `has_live_session`): the row's key handler (`run_config_row`
/// via the Setup `disabled` row) no-ops for both, exactly like the render-side
/// dim (`tests/inline/tui_render_config.rs`). Asserting `toasts.is_empty()`
/// is the load-bearing half here: `disable_profile`'s own gate refuses too
/// (defense in depth), so the flag-unchanged half alone would stay green even
/// without `toggle_profile_disabled`'s own pre-check — only the ABSENCE of a
/// surfaced danger toast proves this gate fired before the backend's `bail!`,
/// keeping the dimmed row truly inert rather than a silent-flag/loud-toast mix.
#[test]
fn disabled_row_toggle_is_inert_for_the_active_account() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
    app.config().state.active_profile = Some("acct".into());
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(&app, "acct"));

    run_config_row(&mut app, ConfigRow::Disabled);

    assert!(
        !app.config().find("acct").unwrap().is_disabled(),
        "the active account's toggle must no-op"
    );
    assert!(
        app.toasts.is_empty(),
        "a gated toggle must stay silent, not surface the backend's own refusal toast"
    );
}

/// Same gate, the other half: a live `clauth start` session also blocks it.
#[test]
fn disabled_row_toggle_is_inert_with_a_live_session() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
    let _pid_guard = arm_live_session(home.home(), "acct");
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(&app, "acct"));

    run_config_row(&mut app, ConfigRow::Disabled);

    assert!(
        !app.config().find("acct").unwrap().is_disabled(),
        "a session-holding account's toggle must no-op"
    );
    assert!(
        app.toasts.is_empty(),
        "a gated toggle must stay silent, not surface the backend's own refusal toast"
    );
}

/// Disabling (real operational impact) reuses `Delete`'s press-to-arm →
/// confirm class through the SAME `armed_action` field: the first
/// `run_config_row` call must only arm the row (no flag flip, no toast), and
/// the second — with the row still armed — actually disables it. Also pins
/// the arm/confirm asymmetry fix: unlike `Delete` (whose confirm removes the
/// row so a stale arm can never resurface), this row survives its own
/// toggle, so the arm must be cleared after firing or a later disable would
/// skip straight past the confirm step.
#[test]
fn disable_row_arms_on_first_press_confirms_on_second() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(&app, "acct"));

    run_config_row(&mut app, ConfigRow::Disabled);
    assert!(
        !app.config().find("acct").unwrap().is_disabled(),
        "the first ⏎ must only arm the row, not flip the flag"
    );
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.armed_action),
        Some(ConfigRow::Disabled),
        "the first ⏎ arms this row"
    );
    assert!(app.toasts.is_empty(), "arming alone must not toast");

    run_config_row(&mut app, ConfigRow::Disabled);
    assert!(
        app.config().find("acct").unwrap().is_disabled(),
        "the second ⏎, while armed, confirms the disable"
    );
    assert!(
        app.toasts.iter().any(|t| t.body.contains("disabled")),
        "the confirmed disable toasts"
    );
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.armed_action),
        None,
        "the arm must clear after firing — this row survives its own toggle, \
         unlike `Delete`, so a stale arm would let a later disable skip the confirm step"
    );
}

/// Enabling is harmless, so it fires immediately on the first ⏎ — never
/// arms, never needs a second press.
#[test]
fn enable_row_fires_immediately_no_arm() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("acct".to_string(), None, None);
    acct.disabled = true;
    let mut app = app_with(vec![acct]);
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(&app, "acct"));

    run_config_row(&mut app, ConfigRow::Disabled);

    assert!(
        !app.config().find("acct").unwrap().is_disabled(),
        "a single ⏎ enables immediately"
    );
    assert!(
        app.toasts.iter().any(|t| t.body.contains("enabled")),
        "the immediate enable toasts"
    );
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.armed_action),
        None,
        "enabling never arms the row"
    );
}

/// The `a` action-menu entry for the `disabled` row flips with the account's
/// state — "disable account" while enabled, "enable account" while disabled
/// — mirroring the row's own label, and still dispatches through
/// `run_config_row` so it obeys the same arm/confirm and immediate-enable
/// rules as pressing ⏎ directly on the row.
#[test]
fn action_menu_labels_the_disabled_row_by_current_state() {
    use super::{
        ActionMenuAction, ConfigFocus, ConfigRow, Tab, build_action_menu, build_draft_existing,
        config_rows, dispatch_action_menu_action,
    };
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
    app.tab = Tab::Setup;
    app.profile_cursor = 0;
    app.config_focus = ConfigFocus::Actions;
    app.config_draft = Some(build_draft_existing(&app, "acct"));
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::Disabled)
        .expect("disable row always present");

    // Enabled account: the menu offers "disable account" and dispatching it
    // only arms the row (same as a direct ⏎), it does not flip the flag yet.
    let menu = build_action_menu(&app);
    assert_eq!(menu.items.len(), 1);
    assert_eq!(menu.items[0].label, "disable account");
    dispatch_action_menu_action(&mut app, ActionMenuAction::DisableProfile);
    assert!(!app.config().find("acct").unwrap().is_disabled());
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.armed_action),
        Some(ConfigRow::Disabled),
        "the menu selection arms exactly like a direct ⏎"
    );

    // Confirm via a direct ⏎ on the now-armed row, then reopen the menu: the
    // label must have flipped to "enable account".
    super::run_config_row(&mut app, ConfigRow::Disabled);
    assert!(app.config().find("acct").unwrap().is_disabled());
    let menu = build_action_menu(&app);
    assert_eq!(menu.items[0].label, "enable account");
    dispatch_action_menu_action(&mut app, ActionMenuAction::EnableProfile);
    assert!(
        !app.config().find("acct").unwrap().is_disabled(),
        "the menu selection enables immediately, same as a direct ⏎"
    );
}

/// The Fallback tab's add-picker (`chain_candidates`) never offers a disabled
/// account — this is the "excluded from any fallback-chain editing UI" half
/// of the spec that isn't a render concern (the selector's dim + chip is
/// covered in `tests/inline/tui_render_chain.rs`).
#[test]
fn chain_candidates_excludes_a_disabled_profile() {
    use super::chain_candidates;
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut disabled = Profile::new("off".to_string(), None, None);
    disabled.disabled = true;
    let enabled = Profile::new("on".to_string(), None, None);
    let app = app_with(vec![disabled, enabled]);

    let candidates = chain_candidates(&app);
    assert!(
        !candidates.iter().any(|n| n == "off"),
        "a disabled profile is never an add-picker candidate: {candidates:?}"
    );
    assert!(
        candidates.iter().any(|n| n == "on"),
        "an enabled, unchained profile still is: {candidates:?}"
    );
}

/// Overview's switch affordance: `request_switch_to` never even offers a
/// disabled account (no confirm modal), matching `switch_profile`'s own
/// shared-guard refusal — never selectable, not just never landed.
#[test]
fn overview_switch_request_never_opens_a_confirm_for_a_disabled_account() {
    use super::{Modal, request_switch_to};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut disabled = Profile::new("off".to_string(), None, None);
    disabled.disabled = true;
    let mut app = app_with(vec![disabled]);

    request_switch_to(&mut app, 0);

    assert!(
        !app.modals.iter().any(|m| matches!(m, Modal::Confirm(_))),
        "a disabled account never raises the switch confirm"
    );
}

/// Rotating a profile a live `clauth start` session owns must not dead-end on a
/// toast (the old behavior): it arms an acknowledge notice explaining the block,
/// and confirming is a no-op — the running session owns rotation, and rotating
/// its already-spent stored token would 400 (`docs/internals.md`, 2026-06-17).
#[test]
fn rotate_tokens_with_live_session_arms_acknowledge_modal() {
    use super::{
        ActionMenuAction, ConfirmAction, Modal, dispatch_action_menu_action, run_confirm_action,
    };
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("busy".to_string(), None, None)]);
    app.profile_cursor = 0;
    let _pid_guard = arm_live_session(home.home(), "busy");

    dispatch_action_menu_action(&mut app, ActionMenuAction::RotateTokens);
    let confirm = app
        .modals
        .last()
        .and_then(|m| match m {
            Modal::Confirm(s) => Some(s),
            _ => None,
        })
        .expect("a live session arms a confirm modal");
    assert!(
        matches!(confirm.on_confirm, ConfirmAction::Acknowledge),
        "a live-session rotate arms an acknowledge notice, not a rotate action"
    );

    let action = confirm.on_confirm.clone();
    run_confirm_action(&mut app, action);
    assert!(
        app.config().find("busy").is_some(),
        "acknowledging the notice leaves the profile untouched"
    );
}

/// No live session: the rotate action arms the normal rotate confirm carrying the
/// per-profile `RotateOne`, not the acknowledge notice.
#[test]
fn rotate_tokens_without_live_session_arms_rotate_confirm() {
    use super::{ActionMenuAction, ConfirmAction, Modal, dispatch_action_menu_action};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("idle".to_string(), None, None)]);
    app.profile_cursor = 0;

    dispatch_action_menu_action(&mut app, ActionMenuAction::RotateTokens);
    let confirm = app
        .modals
        .last()
        .and_then(|m| match m {
            Modal::Confirm(s) => Some(s),
            _ => None,
        })
        .expect("a rotate arms a confirm modal");
    assert!(
        matches!(&confirm.on_confirm, ConfirmAction::RotateOne(n) if n == "idle"),
        "a non-live rotate carries the RotateOne action for the focused profile"
    );
}

/// Minted-credential fixture for the login tests.
fn login_creds(refresh: &str) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

/// A completed login as `login_with` hands it back: the mint plus the account
/// uuid its `/profile` verification probe saw. `uuid` is `None` for a login whose
/// probe failed or returned no usable identity.
fn login_outcome(refresh: &str, uuid: Option<&str>) -> crate::oauth_login::LoginOutcome {
    crate::oauth_login::LoginOutcome {
        credentials: login_creds(refresh),
        account_uuid: uuid.map(str::to_string),
    }
}

/// Like [`login_creds`] but with a caller-chosen access token, so a test can
/// change the live login's fingerprint without changing its account.
fn creds_ra(refresh: &str, access: &str) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

/// Write a plain (diverged) `~/.claude/.credentials.json` carrying `creds`.
fn write_live_creds(creds: &crate::profile::ClaudeCredentials) {
    let path = crate::profile::claude_dir()
        .expect("claude dir")
        .join(".credentials.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(&path, serde_json::to_vec(creds).expect("ser live")).expect("write live");
}

/// Force the 1Hz divergence poll to run now, bypassing its interval throttle.
fn force_poll(app: &mut App) {
    app.last_divergence_check = std::time::Instant::now() - std::time::Duration::from_secs(2);
    super::poll_credentials_divergence(app);
}

/// An in-flight login session fixture at the waiting stage.
fn login_session(name: &str, is_new: bool, generation: u64) -> super::LoginSession {
    super::LoginSession {
        name: name.to_string(),
        is_new,
        generation,
        url: None,
        stage: super::LoginStage::WaitingBrowser,
    }
}

#[test]
fn drain_login_events_discards_a_superseded_result() {
    use super::drain_login_events;
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });

    // The user superseded (or canceled) the first login: the live session now
    // carries generation 2, but a worker for generation 1 is still finishing.
    app.login_generation = 2;
    app.login = Some(login_session("ghost", true, 2));
    app.login_result_tx
        .send((1, Ok(login_outcome("ref", Some("uuid-live")))))
        .unwrap();

    drain_login_events(&mut app);

    assert!(
        app.config().find("ghost").is_none(),
        "a superseded login result must not create a profile"
    );
    assert!(
        app.login.is_some(),
        "the current (gen 2) session stays live; only the stale result is dropped"
    );
}

#[test]
fn login_result_on_the_new_form_stashes_into_the_draft() {
    use super::{ConfigRow, Modal, build_draft_new, config_rows, drain_login_events};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.profile_cursor = 0; // == profile_count() → the `+ new` form
    let mut draft = build_draft_new();
    draft.name = InputState::new("fresh");
    app.config_draft = Some(draft);
    app.login_generation = 1;
    app.login = Some(login_session("fresh", true, 1));
    app.modals.push(Modal::Login);
    app.login_result_tx
        .send((1, Ok(login_outcome("ref", Some("uuid-live")))))
        .unwrap();

    drain_login_events(&mut app);

    assert!(
        app.config().find("fresh").is_none(),
        "capture-then-commit: no profile is persisted until create fires"
    );
    assert!(
        app.config_draft
            .as_ref()
            .is_some_and(|d| d.captured_login.is_some()),
        "the mint lands in the draft"
    );
    assert!(app.login.is_none(), "the session ends with the result");
    assert!(
        !app.modals.iter().any(|m| matches!(m, Modal::Login)),
        "the progress modal closes with the result"
    );
    let rows = config_rows(&app);
    assert_eq!(
        rows.get(app.config_action_cursor),
        Some(&ConfigRow::Create),
        "the cursor lands on `create account`"
    );
}

#[test]
fn relogin_on_a_stashed_new_form_confirms_before_replacing_the_stash() {
    use super::{ConfigFocus, ConfigRow, ConfirmAction, Modal, build_draft_new, run_config_row};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.profile_cursor = 0; // the `+ new` form
    let mut draft = build_draft_new();
    draft.name = InputState::new("fresh");
    // A mint already captured → the `✓ logged in` done-state row.
    draft.captured_login = Some(Box::new(login_outcome("stashed", Some("uuid-stashed"))));
    app.config_draft = Some(draft);
    app.config_focus = ConfigFocus::Actions;

    run_config_row(&mut app, ConfigRow::Login);

    assert!(
        matches!(
            app.modals.last(),
            Some(Modal::Confirm(s)) if matches!(s.on_confirm, ConfirmAction::RestartLogin(_, true))
        ),
        "⏎ on a stashed new-form login must confirm before dropping the capture",
    );
    assert!(
        app.login.is_none(),
        "no login worker starts until the confirm is accepted",
    );
}

#[test]
fn login_result_with_the_form_closed_is_dropped_with_a_warning() {
    use super::{ToastKind, drain_login_events};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.config_draft = None; // form abandoned during the browser round-trip
    app.login_generation = 1;
    app.login = Some(login_session("fresh", true, 1));
    app.login_result_tx
        .send((1, Ok(login_outcome("ref", Some("uuid-live")))))
        .unwrap();

    drain_login_events(&mut app);

    assert!(app.config().find("fresh").is_none());
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Warning && t.body.contains("no longer open")),
        "dropping a real browser round-trip must be surfaced"
    );
}

#[test]
fn commit_new_account_consumes_the_draft_mint() {
    use super::{build_draft_new, commit_new_account};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.profile_cursor = 0;
    let mut draft = build_draft_new();
    draft.name = InputState::new("fresh");
    draft.model = InputState::new("opus");
    draft.captured_login = Some(Box::new(login_outcome("minted", Some("uuid-minted"))));
    app.config_draft = Some(draft);

    commit_new_account(&mut app);

    let cfg = app.config();
    let profile = cfg
        .find("fresh")
        .expect("create account persists the profile");
    assert_eq!(
        profile.refresh_token(),
        Some("minted"),
        "the draft-held mint is saved with the profile"
    );
    assert_eq!(
        profile.models.default.as_deref(),
        Some("opus"),
        "the model row folds into the same create"
    );
    assert_eq!(
        cfg.state.active_profile.as_deref(),
        Some("fresh"),
        "the first profile links and activates like a capture"
    );
    drop(cfg);
    assert_eq!(
        crate::profile_cache::load_profile_cache::<String>(
            "fresh",
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE
        )
        .as_deref(),
        Some("uuid-minted"),
        "the anchor lands under the name the create committed — the draft carried \
         the login's uuid this far precisely because the name was still editable"
    );
}

/// The TUI re-login seeds the identity anchor from the uuid its own `/profile`
/// verification probe already saw — the CLI login has always done this, the TUI
/// login row never did, and an unanchored profile pays a `/profile` every launch
/// (the anchor gate) and can wedge in `auth_broken` once its stored pair dies.
#[test]
fn a_committed_relogin_anchors_the_profile_it_swapped_onto() {
    use super::apply_login;
    use crate::profile::{AppConfig, AppState, DivergenceChoice, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("old"));
    let mut app = App::new(AppConfig {
        state: AppState {
            default_divergence: Some(DivergenceChoice::Overwrite),
            ..AppState::default()
        },
        profiles: vec![work],
    });
    // A reauth that swapped a DIFFERENT account onto the name: the stale anchor
    // must be replaced, or identity would keep proving the old account.
    crate::usage::seed_login_anchor("work", Some("uuid-old-account"));

    apply_login(
        &mut app,
        login_session("work", false, 1),
        login_outcome("new", Some("uuid-new-account")),
    );

    assert_eq!(
        crate::profile_cache::load_profile_cache::<String>(
            "work",
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE
        )
        .as_deref(),
        Some("uuid-new-account"),
        "a committed re-login re-anchors to the account that just authenticated"
    );
}

/// The gated relogin — the DEFAULT path, since `default_divergence` starts unset.
/// Before the user confirms, the stored pair is untouched, so anchoring would
/// claim an identity the profile's credentials can't back (and a wrong anchor is
/// what lets `try_adopt_live_rotation` capture a foreign live login). On confirm,
/// the snapshot carries its uuid into the commit and the anchor follows.
#[test]
fn a_gated_relogin_anchors_only_once_the_user_confirms() {
    use super::{Modal, apply_login, run_confirm_action};
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let anchor = || {
        crate::profile_cache::load_profile_cache::<String>(
            "work",
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        )
    };

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("old"));
    let mut app = App::new(AppConfig {
        state: AppState::default(), // unset divergence default → ask first
        profiles: vec![work],
    });
    // A reauth swapping a DIFFERENT account onto the name.
    crate::usage::seed_login_anchor("work", Some("uuid-old-account"));

    apply_login(
        &mut app,
        login_session("work", false, 1),
        login_outcome("new", Some("uuid-new-account")),
    );

    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("old"),
        "precondition: the overwrite is still gated behind the confirm"
    );
    assert_eq!(
        anchor().as_deref(),
        Some("uuid-old-account"),
        "the anchor must track the STORED credentials, not an unapplied login"
    );

    let Some(Modal::Confirm(state)) = app.modals.pop() else {
        unreachable!("asserted above: the gate opened a confirm");
    };
    run_confirm_action(&mut app, state.on_confirm);

    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("new"),
        "precondition: the confirmed relogin committed the swap"
    );
    assert_eq!(
        anchor().as_deref(),
        Some("uuid-new-account"),
        "the confirmed relogin re-anchors — the uuid rode the snapshot into the commit"
    );
}

#[test]
fn login_stage_events_advance_the_session() {
    use super::{LoginEvent, LoginStage, drain_login_events};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.login_generation = 1;
    app.login = Some(login_session("fresh", true, 1));

    app.login_event_tx
        .send((1, LoginEvent::Url("https://example.test/auth".to_string())))
        .unwrap();
    app.login_event_tx
        .send((1, LoginEvent::Stage(LoginStage::ExchangingCode)))
        .unwrap();
    // A stale generation's stage bump is ignored.
    app.login_event_tx
        .send((7, LoginEvent::Stage(LoginStage::Verifying)))
        .unwrap();

    drain_login_events(&mut app);

    let session = app.login.as_ref().expect("session stays live");
    assert_eq!(session.url.as_deref(), Some("https://example.test/auth"));
    assert_eq!(session.stage, LoginStage::ExchangingCode);
}

#[test]
fn login_modal_esc_collapses_without_canceling() {
    use super::{KeyCode, Modal, handle_key, start_login};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.login_generation = 1;
    app.login = Some(login_session("fresh", true, 1));
    app.modals.push(Modal::Login);

    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(app.modals.is_empty(), "esc pops the modal");
    assert!(
        app.login.is_some(),
        "the login keeps running while collapsed"
    );
    assert_eq!(
        app.login_generation, 1,
        "collapsing must not bump the generation"
    );

    // ⏎ on the login row while one is in flight re-expands instead of
    // starting a second login.
    start_login(&mut app, "other".to_string(), false);
    assert!(
        app.modals.iter().any(|m| matches!(m, Modal::Login)),
        "a repeat login request reopens the progress modal"
    );
    assert_eq!(
        app.login.as_ref().map(|s| s.name.as_str()),
        Some("fresh"),
        "the in-flight session is untouched"
    );
    app.modals.clear();

    // Collapsed, top-level q cancels too (symmetric with esc) — it must not
    // arm the 2-step quit or ascend out of a Setup form while a login runs.
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('q')));
    assert!(app.login.is_none(), "top-level q cancels the login");
    assert!(!app.quit, "canceling a login must not quit the app");

    // And esc is the equivalent cancel path.
    app.login_generation = 2;
    app.login = Some(login_session("fresh", true, 2));
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(app.login.is_none(), "top-level esc cancels the login");
}

#[test]
fn relogin_gate_maps_divergence_defaults() {
    use super::{ConfirmAction, Modal, apply_login};
    use crate::profile::{AppConfig, AppState, DivergenceChoice, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let profile_with = |refresh: &str| {
        let mut p = Profile::new("work".to_string(), None, None);
        p.credentials = Some(login_creds(refresh));
        p
    };

    // Unset default (ask) → confirm modal, stored creds untouched.
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile_with("old")],
    });
    apply_login(
        &mut app,
        login_session("work", false, 1),
        login_outcome("new", Some("uuid-new")),
    );
    assert!(
        matches!(
            app.modals.last(),
            Some(Modal::Confirm(state))
                if matches!(&state.on_confirm, ConfirmAction::CaptureOverwrite(_, name, false) if name == "work")
        ),
        "an unset divergence default must ask before overwriting"
    );
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("old"),
        "stored creds stay until the user confirms"
    );
    // Confirming actually lands the deferred overwrite.
    let Some(Modal::Confirm(state)) = app.modals.pop() else {
        unreachable!("asserted above");
    };
    super::run_confirm_action(&mut app, state.on_confirm);
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("new"),
        "the confirmed re-login replaces the stored creds"
    );

    // NewProfile / Discard defaults also ask — only Overwrite applies silently.
    for choice in [DivergenceChoice::NewProfile, DivergenceChoice::Discard] {
        let mut app = App::new(AppConfig {
            state: AppState {
                default_divergence: Some(choice),
                ..AppState::default()
            },
            profiles: vec![profile_with("old")],
        });
        apply_login(
            &mut app,
            login_session("work", false, 1),
            login_outcome("new", Some("uuid-new")),
        );
        assert!(
            matches!(app.modals.last(), Some(Modal::Confirm(_))),
            "{choice:?} must gate the overwrite behind a confirm"
        );
    }

    // Overwrite default → applied immediately, no modal.
    let mut app = App::new(AppConfig {
        state: AppState {
            default_divergence: Some(DivergenceChoice::Overwrite),
            ..AppState::default()
        },
        profiles: vec![profile_with("old")],
    });
    apply_login(
        &mut app,
        login_session("work", false, 1),
        login_outcome("new", Some("uuid-new")),
    );
    assert!(
        app.modals.is_empty(),
        "an Overwrite default applies silently"
    );
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("new"),
        "the re-login replaced the stored creds"
    );

    // Credential-less profile: nothing diverges → silent apply even when unset.
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![Profile::new("work".to_string(), None, None)],
    });
    apply_login(
        &mut app,
        login_session("work", false, 1),
        login_outcome("new", Some("uuid-new")),
    );
    assert!(
        app.modals.is_empty(),
        "no stored creds means no divergence to gate on"
    );
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("new"),
        "the first login adopts silently"
    );
}

// A divergence must never lock the TUI: the 1Hz poll raises the non-blocking
// BANNER (`divergence_pending`), never the modal — browsing usage stays fully
// available. <kbd>d</kbd> opens the resolver on demand; Esc closes it and, with
// no auto-push left, nothing re-raises it. (Supersedes the issue #20 snooze:
// with no auto-push there is nothing to snooze.)
#[test]
fn divergence_flags_the_banner_and_never_blocks_the_tui() {
    use super::{Modal, handle_key};
    use crate::profile::{AppConfig, AppState, Profile, save_profile};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_profile(&work).expect("save work");
    write_live_creds(&creds_ra("rt-live", "at-1"));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            ..AppState::default()
        },
        profiles: vec![work],
    });

    force_poll(&mut app);
    assert!(
        app.modals.is_empty(),
        "the poll must NOT raise the modal — a divergence can't lock the TUI"
    );
    let notice = app
        .divergence_pending
        .clone()
        .expect("the poll flags the banner instead");
    assert_eq!(notice.active, "work");
    assert_eq!(
        notice.sibling, None,
        "an unknown login has no owner to offer"
    );

    // `d` opens the resolver on demand; Esc closes it and nothing re-raises.
    handle_key(&mut app, key(KeyCode::Char('d')));
    assert!(
        matches!(app.modals.last(), Some(Modal::Divergence(_))),
        "d opens the resolver from the banner"
    );
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(app.modals.is_empty(), "esc dismisses the resolver");
    force_poll(&mut app);
    assert!(app.modals.is_empty(), "no auto-re-raise after dismissal");

    // The link healing clears the banner (and `d` becomes a no-op).
    crate::claude::force_link_profile_credentials("work").expect("relink");
    force_poll(&mut app);
    assert!(
        app.divergence_pending.is_none(),
        "a clean link clears the banner"
    );
    handle_key(&mut app, key(KeyCode::Char('d')));
    assert!(app.modals.is_empty(), "d is a no-op with no divergence");
}

/// Claude Code's logged-out shell (both tokens blanked after its own refresh
/// died) is not an unsaved login: the poll must not flag the banner, and a
/// configured `default_divergence` must never "capture" the empty tokens over
/// the profile's stored chain.
#[test]
fn divergence_poll_ignores_a_logged_out_shell() {
    use crate::profile::{AppConfig, AppState, DivergenceChoice, Profile, save_profile};
    let _home = crate::testutil::HomeSandbox::new();

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_profile(&work).expect("save work");
    // CC's logged-out shell: both tokens blanked. Still classifies Diverged.
    write_live_creds(&creds_ra("", ""));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            profiles: vec!["work".into()],
            // The most dangerous configuration: an auto-resolving default
            // that would capture the live file into the profile.
            default_divergence: Some(DivergenceChoice::Overwrite),
            ..AppState::default()
        },
        profiles: vec![work],
    });

    force_poll(&mut app);
    assert!(
        app.divergence_pending.is_none(),
        "an empty shell is nothing to resolve — no banner"
    );
    assert!(app.modals.is_empty(), "and certainly no modal");
    let stored = crate::profile::profile_dir("work")
        .expect("work dir")
        .join("credentials.json");
    let stored: crate::profile::ClaudeCredentials =
        crate::profile::read_json_file(&stored).expect("read work store");
    assert_eq!(
        stored.refresh_token(),
        Some("rt-work"),
        "the divergence default must never capture blank tokens over the stored login"
    );
}

/// A clauth-owned symlink in the live slot is never "unsaved credentials": a
/// long-lived `session-token.json` for the active profile flips its install
/// source, so the live symlink classifies Diverged though re-pointing it loses
/// no login. The 1Hz poll must NOT flag the banner — it repainted it every second.
#[cfg(unix)]
#[test]
fn divergence_poll_ignores_a_stale_clauth_symlink() {
    use crate::profile::{
        AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, save_profile,
    };
    let _home = crate::testutil::HomeSandbox::new();

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_profile(&work).expect("save work");
    // The live slot is clauth's own symlink into work's rotating store.
    crate::claude::force_link_profile_credentials("work").expect("link work");
    // A long-lived session token (no refresh token) flips work's install source;
    // the stale symlink still points at credentials.json, so classify reads
    // Diverged with nothing unsaved behind it.
    let sidecar = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat-work".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };
    let work_dir = crate::profile::profile_dir("work").expect("work dir");
    std::fs::write(
        work_dir.join("session-token.json"),
        serde_json::to_vec(&sidecar).expect("ser sidecar"),
    )
    .expect("write session-token sidecar");

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            profiles: vec!["work".into()],
            ..AppState::default()
        },
        profiles: vec![work],
    });

    force_poll(&mut app);
    assert!(
        app.divergence_pending.is_none(),
        "a clauth-owned symlink is nothing to resolve — no 1Hz banner"
    );
    assert!(app.modals.is_empty(), "and certainly no modal");
}

/// The macOS steady-state twin: after a switch, Claude Code rewrites the live
/// slot as a REGULAR-FILE mirror of the Keychain, so the stale-sidecar state the
/// test above pins as a symlink recurs as a regular file. The 1Hz poll must
/// still stay silent — the mirror's login is saved in `credentials.json`. A
/// symlink-identity check reads the regular file as divergence and repaints the
/// banner every second; the content-based exemption clears it.
#[test]
fn divergence_poll_ignores_a_macos_regular_file_mirror() {
    use crate::profile::{
        AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, save_profile,
    };
    let _home = crate::testutil::HomeSandbox::new();

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_profile(&work).expect("save work");
    // CC's regular-file mirror: work's stored login as a plain file (access token
    // "acc", same as work's credentials.json), NOT our symlink.
    write_live_creds(&login_creds("rt-work"));
    // The sidecar flips work's install source; the mirror now classifies Diverged
    // with nothing unsaved behind it.
    let sidecar = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat-work".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };
    let work_dir = crate::profile::profile_dir("work").expect("work dir");
    std::fs::write(
        work_dir.join("session-token.json"),
        serde_json::to_vec(&sidecar).expect("ser sidecar"),
    )
    .expect("write session-token sidecar");

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            profiles: vec!["work".into()],
            ..AppState::default()
        },
        profiles: vec![work],
    });

    force_poll(&mut app);
    assert!(
        app.divergence_pending.is_none(),
        "a saved login mirrored as a regular file is nothing to resolve — no 1Hz banner"
    );
    assert!(app.modals.is_empty(), "and certainly no modal");
}

/// The banner and the resolver both identify the live login's OWNER when it is
/// a stored sibling — by exact token match here (the half-landed-switch shape)
/// — and the resolver leads with the "switch to it" action.
#[test]
fn divergence_identifies_a_sibling_owner_and_leads_with_switch_to_it() {
    use super::{ConfirmAction, DivergenceAction, Modal, handle_key};
    use crate::profile::{AppConfig, AppState, DivergenceChoice, Profile, save_profile};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_profile(&work).expect("save work");
    let mut play = Profile::new("play".to_string(), None, None);
    play.credentials = Some(creds_ra("rt-play", "at-play"));
    save_profile(&play).expect("save play");
    // The live file carries play's EXACT stored pair while work is active.
    write_live_creds(&creds_ra("rt-play", "at-play"));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            profiles: vec!["work".into(), "play".into()],
            ..AppState::default()
        },
        profiles: vec![work, play],
    });

    force_poll(&mut app);
    let notice = app.divergence_pending.clone().expect("banner flagged");
    assert_eq!(notice.sibling.as_deref(), Some("play"));

    handle_key(&mut app, key(KeyCode::Char('d')));
    let Some(Modal::Divergence(form)) = app.modals.last() else {
        panic!("d opens the resolver");
    };
    assert_eq!(form.sibling.as_deref(), Some("play"));
    let actions = form.actions();
    assert_eq!(
        actions.first(),
        Some(&DivergenceAction::SwitchToOwner("play".to_string())),
        "the owner switch leads the menu"
    );
    assert_eq!(actions.len(), 4, "the three generic choices follow");
    assert_eq!(
        actions[1],
        DivergenceAction::Choice(DivergenceChoice::Overwrite)
    );

    // Enter on the leading SwitchToOwner action raises the AdoptDivergence
    // confirm for the owner — the near-always-right resolution, one keypress.
    handle_key(&mut app, key(KeyCode::Enter));
    let Some(Modal::Confirm(confirm)) = app.modals.last() else {
        panic!("enter on switch-to-owner raises the adopt confirm");
    };
    assert!(
        matches!(&confirm.on_confirm, ConfirmAction::AdoptDivergence(_, owner) if owner == "play"),
        "the confirm adopts the live login into its owner 'play'",
    );
    assert!(!confirm.choice, "the adopt confirm defaults to cancel");
}

/// A flagged divergence renders through the ONE system banner (`update_banner`),
/// not a bespoke Overview-only line: a WARNING banner naming the owner when
/// known, cleared the moment the link heals. Guards the banner-refactor codepath.
#[test]
fn divergence_renders_through_the_system_banner() {
    use super::{BannerSeverity, update_banner};
    use crate::profile::{AppConfig, AppState, Profile, save_profile};
    let _home = crate::testutil::HomeSandbox::new();

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_profile(&work).expect("save work");
    let mut play = Profile::new("play".to_string(), None, None);
    play.credentials = Some(creds_ra("rt-play", "at-play"));
    save_profile(&play).expect("save play");
    // Live file carries play's EXACT stored pair while work is active → owner known.
    write_live_creds(&creds_ra("rt-play", "at-play"));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            profiles: vec!["work".into(), "play".into()],
            ..AppState::default()
        },
        profiles: vec![work, play],
    });

    force_poll(&mut app);
    update_banner(&mut app);
    let banner = app
        .banner
        .as_ref()
        .expect("divergence raises the system banner");
    assert_eq!(banner.severity, BannerSeverity::Warning);
    assert_eq!(
        banner.message,
        "live login is 'play' · not the active 'work' · press d to resolve",
    );

    // Heal the link → the divergence clears and so does the banner.
    crate::claude::force_link_profile_credentials("work").expect("relink");
    force_poll(&mut app);
    update_banner(&mut app);
    assert!(
        app.banner.is_none(),
        "a clean link clears the divergence banner",
    );
}

// Issue #20: "save elsewhere" must let the user route the live login into a
// profile OTHER than the active one, so re-logging a second account while the
// wrong profile is active no longer forces two profiles onto one account.
#[test]
fn divergence_picker_saves_the_login_into_a_chosen_profile() {
    use super::{ConfirmAction, Modal, handle_key, run_confirm_action, run_divergence_choice};
    use crate::profile::{AppConfig, AppState, DivergenceChoice, Profile, save_profile};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_profile(&work).expect("save work");
    let mut spare = Profile::new("spare".to_string(), None, None);
    spare.credentials = Some(login_creds("rt-spare"));
    save_profile(&spare).expect("save spare");
    // CC re-logged an account; the live file carries a fresh token that matches
    // no stored profile (a re-login mints a new refresh token).
    write_live_creds(&creds_ra("rt-fresh", "at-fresh"));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            ..AppState::default()
        },
        profiles: vec![work, spare],
    });

    // "save elsewhere" opens the picker listing only the non-active profile.
    run_divergence_choice(&mut app, "work", DivergenceChoice::NewProfile);
    let Some(Modal::DivergenceTarget(form)) = app.modals.last() else {
        panic!("expected the target picker, got {:?}", app.modals.last());
    };
    assert_eq!(
        form.targets,
        vec!["spare".to_string()],
        "the active profile is never an overwrite target"
    );

    // Move to "spare" (row 1) and pick it.
    handle_key(&mut app, key(KeyCode::Down));
    handle_key(&mut app, key(KeyCode::Enter));
    let Some(Modal::Confirm(state)) = app.modals.last() else {
        panic!(
            "expected the overwrite confirm, got {:?}",
            app.modals.last()
        );
    };
    assert!(
        matches!(&state.on_confirm, ConfirmAction::AdoptDivergence(_, name) if name == "spare"),
        "the confirm adopts the live login into the chosen profile"
    );

    let Some(Modal::Confirm(state)) = app.modals.pop() else {
        unreachable!("asserted above");
    };
    run_confirm_action(&mut app, state.on_confirm);

    assert_eq!(
        app.config().find("spare").and_then(|p| p.refresh_token()),
        Some("rt-fresh"),
        "the live login landed in the chosen profile"
    );
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("rt-work"),
        "the previously active profile is untouched"
    );
    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some("spare"),
        "the chosen profile becomes active so the divergence is resolved"
    );
}

/// A configured `default_divergence` is owner-gated: it may only resolve a login
/// no stored sibling owns. An owner-blind default captures a SIBLING profile's
/// re-login into the active profile — credential misattribution the user never
/// gets a say in. A sibling-owned divergence falls through to the banner, whose
/// "switch to it" action is the right resolution.
#[test]
fn divergence_default_never_captures_a_sibling_owned_login() {
    use super::{StartupSignal, drain_startup_signals};
    use crate::profile::{AppConfig, AppState, DivergenceChoice, Profile, save_profile};
    let _home = crate::testutil::HomeSandbox::new();

    // work is active; the live file carries play's EXACT stored pair (the
    // half-landed-switch / sibling-re-login shape).
    let sibling_owned_app = |default: DivergenceChoice| {
        let mut work = Profile::new("work".to_string(), None, None);
        work.credentials = Some(creds_ra("rt-work", "at-work"));
        save_profile(&work).expect("save work");
        let mut play = Profile::new("play".to_string(), None, None);
        play.credentials = Some(creds_ra("rt-play", "at-play"));
        save_profile(&play).expect("save play");
        write_live_creds(&creds_ra("rt-play", "at-play"));
        App::new(AppConfig {
            state: AppState {
                active_profile: Some("work".into()),
                profiles: vec!["work".into(), "play".into()],
                default_divergence: Some(default),
                ..AppState::default()
            },
            profiles: vec![work, play],
        })
    };

    // Overwrite default + sibling-owned login: no capture, banner instead.
    let mut app = sibling_owned_app(DivergenceChoice::Overwrite);
    force_poll(&mut app);
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("rt-work"),
        "an Overwrite default must not capture play's login into work"
    );
    assert_eq!(
        app.config().find("play").and_then(|p| p.refresh_token()),
        Some("rt-play"),
        "play's stored creds are untouched"
    );
    assert_eq!(
        app.divergence_pending
            .as_ref()
            .and_then(|n| n.sibling.as_deref()),
        Some("play"),
        "the sibling-owner banner is offered instead of the default"
    );
    assert!(app.modals.is_empty(), "the banner never becomes a modal");

    // NewProfile default: same gate — no target picker, banner instead.
    let mut app = sibling_owned_app(DivergenceChoice::NewProfile);
    force_poll(&mut app);
    assert!(
        app.modals.is_empty(),
        "a NewProfile default must not open the picker on a sibling-owned login"
    );
    assert_eq!(
        app.divergence_pending
            .as_ref()
            .and_then(|n| n.sibling.as_deref()),
        Some("play"),
    );

    // The startup reconcile path resolves defaults through the same gate.
    let mut app = sibling_owned_app(DivergenceChoice::Overwrite);
    app.startup_sender
        .send(StartupSignal::ReconcileNeedsPrompt {
            active: "work".to_string(),
        })
        .expect("send reconcile signal");
    drain_startup_signals(&mut app);
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("rt-work"),
        "the startup reconcile default is owner-gated too"
    );
    assert_eq!(
        app.divergence_pending
            .as_ref()
            .and_then(|n| n.sibling.as_deref()),
        Some("play"),
        "startup flags the sibling banner"
    );

    // Other direction: an owner-UNKNOWN (foreign) login still auto-resolves.
    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(creds_ra("rt-work", "at-work"));
    save_profile(&work).expect("save work");
    write_live_creds(&creds_ra("rt-fresh", "at-fresh"));
    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            profiles: vec!["work".into()],
            default_divergence: Some(DivergenceChoice::Overwrite),
            ..AppState::default()
        },
        profiles: vec![work],
    });
    force_poll(&mut app);
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("rt-fresh"),
        "no sibling owns the login, so the Overwrite default applies as before"
    );
    assert!(
        app.divergence_pending.is_none(),
        "the resolved default leaves no banner behind"
    );
    assert!(app.modals.is_empty(), "an Overwrite default asks nothing");
}

#[test]
fn compact_entry_sets_flag_no_toast() {
    let mut app = bare_app();
    app.update_compact(13);
    assert!(app.compact);
    assert!(app.toasts.is_empty(), "compact must not fire a toast");
}

#[test]
fn compact_yields_warning_banner() {
    use super::{BannerSeverity, update_banner};
    let mut app = bare_app();
    app.update_compact(13);
    update_banner(&mut app);
    let banner = app.banner.as_ref().expect("compact banner present");
    assert_eq!(banner.severity, BannerSeverity::Warning);
    assert_eq!(
        banner.message,
        "terminal too small · enlarge for full layout"
    );
}

#[test]
fn compact_exit_clears_banner() {
    use super::update_banner;
    let mut app = bare_app();
    app.update_compact(13);
    update_banner(&mut app);
    assert!(app.banner.is_some());
    app.update_compact(14);
    update_banner(&mut app);
    assert!(!app.compact);
    assert!(app.banner.is_none(), "banner self-clears on resize");
}

#[test]
fn compact_rearm_after_exit() {
    use super::update_banner;
    let mut app = bare_app();
    app.update_compact(13);
    app.update_compact(14);
    app.update_compact(13);
    update_banner(&mut app);
    assert!(app.compact);
    assert!(app.toasts.is_empty(), "compact must not fire a toast");
    assert!(app.banner.is_some());
}

// ── global config tab ────────────────────────────────────────────────────

use super::theme::{self, Tier};
use super::{GLOBAL_CONFIG_ROWS, GlobalConfigRow, KeyCode, Tab};

use crate::testutil::{TierSandbox, key};

#[test]
fn theme_set_tier_round_trips() {
    // The pin's own store is the first leg; the guard exists so the last leg
    // does not outlive this test.
    let _tier = TierSandbox::new(Tier::Full);
    assert_eq!(theme::tier(), Tier::Full);
    theme::set_tier(Tier::Compatible);
    assert_eq!(theme::tier(), Tier::Compatible);
    theme::set_tier(Tier::Full);
    assert_eq!(theme::tier(), Tier::Full);
}

#[test]
fn global_config_cursor_wraps() {
    let mut app = bare_app();
    app.tab = Tab::Config;
    let last = GLOBAL_CONFIG_ROWS.len() - 1;

    assert_eq!(app.global_config_cursor, 0);
    super::handle_global_config_key(&mut app, key(KeyCode::Up));
    assert_eq!(
        app.global_config_cursor, last,
        "Up from first wraps to last"
    );
    super::handle_global_config_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.global_config_cursor, 0, "Down from last wraps to first");
}

// ── divergence default ─────────────────────────────────────────────────────

use crate::profile::DivergenceChoice;

#[test]
fn next_divergence_default_cycles_round_trip() {
    assert_eq!(
        super::next_divergence_default(None),
        Some(DivergenceChoice::Overwrite)
    );
    assert_eq!(
        super::next_divergence_default(Some(DivergenceChoice::Overwrite)),
        Some(DivergenceChoice::NewProfile)
    );
    assert_eq!(
        super::next_divergence_default(Some(DivergenceChoice::NewProfile)),
        Some(DivergenceChoice::Discard)
    );
    assert_eq!(
        super::next_divergence_default(Some(DivergenceChoice::Discard)),
        None
    );
}

#[test]
fn divergence_default_row_is_reachable_by_cursor() {
    let mut app = bare_app();
    app.tab = Tab::Config;
    let pos = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::DivergenceDefault)
        .unwrap();

    app.global_config_cursor = pos;
    let from_up = if pos == 0 {
        GLOBAL_CONFIG_ROWS.len() - 1
    } else {
        pos - 1
    };
    super::handle_global_config_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.global_config_cursor, from_up);
    super::handle_global_config_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.global_config_cursor, pos);
}

// ── burn-aware switching (issue #8 follow-up b) ─────────────────────────────

#[test]
fn burn_aware_row_is_reachable_by_cursor() {
    let mut app = bare_app();
    app.tab = Tab::Config;
    let pos = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::BurnAware)
        .unwrap();

    app.global_config_cursor = pos;
    let from_up = if pos == 0 {
        GLOBAL_CONFIG_ROWS.len() - 1
    } else {
        pos - 1
    };
    super::handle_global_config_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.global_config_cursor, from_up);
    super::handle_global_config_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.global_config_cursor, pos);
}

#[test]
fn burn_aware_space_toggles_and_persists() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::BurnAware)
        .unwrap();
    assert!(!app.config().state.burn_aware_switching, "off by default");

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        app.config().state.burn_aware_switching,
        "space toggles the mode on"
    );

    // Persisted to profiles.toml, not just the in-memory config — reload it
    // fresh, the way a relaunch would pick up the flag.
    let reloaded: crate::profile::AppState = toml::from_str(
        &std::fs::read_to_string(crate::profile::clauth_dir().unwrap().join("profiles.toml"))
            .expect("read profiles.toml"),
    )
    .expect("parse profiles.toml");
    assert!(reloaded.burn_aware_switching, "toggle persists to disk");

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        !app.config().state.burn_aware_switching,
        "space toggles the mode back off"
    );
}

// ── spend budget (real money) ───────────────────────────────────────────────

#[test]
fn spend_budget_space_toggles_and_persists() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::SpendBudget)
        .unwrap();
    assert!(
        !app.config().state.spend_budget_switching,
        "money is never spent unless asked for: off by default"
    );

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(app.config().state.spend_budget_switching, "space arms it");

    let reloaded: crate::profile::AppState = toml::from_str(
        &std::fs::read_to_string(crate::profile::clauth_dir().unwrap().join("profiles.toml"))
            .expect("read profiles.toml"),
    )
    .expect("parse profiles.toml");
    assert!(reloaded.spend_budget_switching, "toggle persists to disk");

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        !app.config().state.spend_budget_switching,
        "space toggles it back off"
    );
}

// `money spent` is its own row, not an alias of `quota spent`: staying is free
// when quota runs out and costs money when a budget does, so the two must be
// settable in opposite directions.
#[test]
fn budget_wrap_off_space_toggles_and_persists() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::SwitchOffWhenBudgetSpent)
        .unwrap();
    assert!(
        app.config().state.switch_off_when_budget_spent,
        "a spent budget stops spending unless told otherwise: on by default"
    );
    assert!(
        !app.config().state.switch_off_when_spent,
        "...while `quota spent` defaults the other way, since staying is free there"
    );

    // `money spent` is inert (dimmed) until spend budget is armed — arm it first,
    // then space toggles it.
    {
        let mut cfg = app.config();
        cfg.state.spend_budget_switching = true;
    }
    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        !app.config().state.switch_off_when_budget_spent,
        "space flips it to stay on active"
    );
    assert!(
        !app.config().state.switch_off_when_spent,
        "flipping the budget row must not touch `quota spent`"
    );

    let reloaded: crate::profile::AppState = toml::from_str(
        &std::fs::read_to_string(crate::profile::clauth_dir().unwrap().join("profiles.toml"))
            .expect("read profiles.toml"),
    )
    .expect("parse profiles.toml");
    assert!(
        !reloaded.switch_off_when_budget_spent,
        "toggle persists to disk"
    );
}

// `money spent` decides no halt while spend budget is off (nothing spends), so
// it renders dimmed AND is a true disabled row: space/⏎ must no-op, or `faint`
// would stop meaning "inert".
#[test]
fn money_spent_is_inert_while_spend_budget_is_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    assert!(
        !app.config().state.spend_budget_switching,
        "spend budget off by default — the money-spent row is inert"
    );
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::SwitchOffWhenBudgetSpent)
        .unwrap();
    let before = app.config().state.switch_off_when_budget_spent;

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config().state.switch_off_when_budget_spent,
        before,
        "space must not cycle an inert row"
    );
    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.config().state.switch_off_when_budget_spent,
        before,
        "enter must not cycle an inert row either"
    );
}

// Same inert guard, but entered through the REAL top-level router (`handle_key`
// → tab dispatch), the layer a keystroke actually hits. A sub-handler-only test
// stays green if the inert check ever moves above `handle_global_config_key`.
#[test]
fn money_spent_is_inert_through_the_top_level_router() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::SwitchOffWhenBudgetSpent)
        .unwrap();
    let before = app.config().state.switch_off_when_budget_spent;

    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config().state.switch_off_when_budget_spent,
        before,
        "space through handle_key must not cycle an inert row"
    );

    // Positive control: arm spend budget and the SAME key path must now cycle the
    // row — proves `handle_key` actually routes space here, so the inert "no
    // change" above is the guard doing its job, not a router that never arrives.
    {
        let mut cfg = app.config();
        cfg.state.spend_budget_switching = true;
    }
    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    assert_ne!(
        app.config().state.switch_off_when_budget_spent,
        before,
        "space through handle_key cycles the row once spend budget is armed"
    );
}

// ── preemptive rotation (rotation coherence #1) ─────────────────────────────

#[test]
fn preemptive_rotation_space_toggles_on_macos_inert_elsewhere() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::PreemptiveRotation)
        .unwrap();
    assert!(
        !app.config().state.preemptive_rotation,
        "off by default — stock stays strictly lazy"
    );

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));

    // Off macOS the Keychain mirror is never live, so preemptive rotation can't
    // fire and the row is a disabled no-op; on macOS the toggle works + persists.
    if cfg!(target_os = "macos") {
        assert!(
            app.config().state.preemptive_rotation,
            "space toggles the mode on"
        );
        // Persisted to profiles.toml — the scheduler reads the flag off the
        // shared config, but a relaunch must pick it up from disk too.
        let reloaded: crate::profile::AppState = toml::from_str(
            &std::fs::read_to_string(crate::profile::clauth_dir().unwrap().join("profiles.toml"))
                .expect("read profiles.toml"),
        )
        .expect("parse profiles.toml");
        assert!(reloaded.preemptive_rotation, "toggle persists to disk");

        super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
        assert!(
            !app.config().state.preemptive_rotation,
            "space toggles the mode back off"
        );
    } else {
        assert!(
            !app.config().state.preemptive_rotation,
            "inert off macOS — space must not toggle a row that can't fire"
        );
    }
}

// ── refresh interval custom value ──────────────────────────────────────────

use super::parse_refresh_secs;

/// Park the Config cursor on the refresh-interval row.
fn on_refresh_row(app: &mut App) {
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::RefreshInterval)
        .unwrap();
}

#[test]
fn parse_refresh_secs_accepts_in_range_only() {
    // Whole seconds, scaled to ms, must land in 10s..=3600s.
    assert_eq!(parse_refresh_secs("10"), Some(10_000));
    assert_eq!(parse_refresh_secs("90"), Some(90_000));
    assert_eq!(parse_refresh_secs("3600"), Some(3_600_000));
    assert!(parse_refresh_secs("9").is_none(), "below the 10s floor");
    assert!(parse_refresh_secs("3601").is_none(), "above the 1h cap");
    assert!(parse_refresh_secs("-5").is_none());
    assert!(parse_refresh_secs("1.5").is_none());
    assert!(parse_refresh_secs("abc").is_none());
    assert!(parse_refresh_secs("").is_none());
}

#[test]
fn refresh_interval_enter_opens_editor_seeded_in_seconds() {
    let mut app = bare_app();
    on_refresh_row(&mut app);

    assert!(app.refresh_interval_draft.is_none());
    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    let draft = app
        .refresh_interval_draft
        .as_ref()
        .expect("⏎ opens the custom-value editor");
    assert_eq!(draft.value, "90", "seeded with the default 90s in seconds");
}

#[test]
fn refresh_interval_space_cycles_without_opening_editor() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_refresh_row(&mut app);
    let before = app.refresh_interval.load(Ordering::Relaxed);

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        app.refresh_interval_draft.is_none(),
        "space cycles presets, never opens the editor"
    );
    assert_ne!(
        app.refresh_interval.load(Ordering::Relaxed),
        before,
        "space steps to the next preset"
    );
}

#[test]
fn refresh_interval_space_wraps_top_preset_to_min() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_refresh_row(&mut app);
    app.refresh_interval.store(300_000, Ordering::Relaxed); // top preset

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        15_000,
        "space at the top preset wraps to the first preset, never clamps"
    );
}

#[test]
fn refresh_interval_space_from_custom_lands_on_next_preset() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_refresh_row(&mut app);
    app.refresh_interval.store(45_000, Ordering::Relaxed); // custom, between 30s and 60s

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        60_000,
        "space from an off-ladder custom value steps to the next preset above it, not past it"
    );
}

#[test]
fn refresh_interval_plus_minus_are_unbound() {
    let _home = crate::testutil::HomeSandbox::new();

    // Checked in isolation: pressing `+` then `-` would cancel back to the
    // starting preset even if both still worked, which would hide a
    // regression. Each key is asserted alone against a fresh app.
    let mut app = bare_app();
    on_refresh_row(&mut app);
    let before = app.refresh_interval.load(Ordering::Relaxed);
    super::handle_global_config_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        before,
        "+ no longer steps the refresh preset; removed in favor of space-only cycling"
    );
    assert!(
        app.refresh_interval_draft.is_none(),
        "+ must not open the custom-value editor either"
    );

    let mut app = bare_app();
    on_refresh_row(&mut app);
    let before = app.refresh_interval.load(Ordering::Relaxed);
    super::handle_global_config_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        before,
        "- no longer steps the refresh preset either"
    );
}

#[test]
fn refresh_interval_custom_value_commits_and_clears() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_refresh_row(&mut app);

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    // Clear the seeded "90", type "45".
    super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Backspace));
    super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Backspace));
    for c in "45".chars() {
        super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Enter));

    assert!(
        app.refresh_interval_draft.is_none(),
        "a valid commit clears the draft"
    );
    assert_eq!(app.refresh_interval.load(Ordering::Relaxed), 45_000);
}

#[test]
fn refresh_interval_out_of_range_keeps_editor_open() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_refresh_row(&mut app);
    let before = app.refresh_interval.load(Ordering::Relaxed);

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    for c in "99999".chars() {
        super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Enter));

    assert!(
        app.refresh_interval_draft.is_some(),
        "an out-of-range value keeps the editor open for correction"
    );
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        before,
        "interval stays put while the typed value is invalid"
    );
}

#[test]
fn refresh_interval_esc_discards_editor() {
    let mut app = bare_app();
    on_refresh_row(&mut app);
    let before = app.refresh_interval.load(Ordering::Relaxed);

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    for c in "30".chars() {
        super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Esc));

    assert!(
        app.refresh_interval_draft.is_none(),
        "esc discards the editor"
    );
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        before,
        "esc leaves the interval unchanged"
    );
}

// ── Setup tab: per-account custom env editor ───────────────────────────────

mod env_editor {
    use super::super::{App, ConfigFocus, ConfigRow, InputState, Modal, Tab, config_rows};
    use crate::profile::{AppConfig, AppState, Profile};
    use crate::testutil::HomeSandbox;
    use std::collections::BTreeMap;

    fn app_with_env(env: BTreeMap<String, String>) -> App {
        let mut profile = Profile::new("acct".to_string(), None, None);
        profile.env = env;
        App::new(AppConfig {
            state: AppState::default(),
            profiles: vec![profile],
        })
    }

    fn enter_detail(app: &mut App) {
        app.tab = Tab::Setup;
        app.profile_cursor = 0;
        super::super::enter_config_detail(app);
        assert_eq!(app.config_focus, ConfigFocus::Actions);
    }

    #[test]
    fn config_rows_insert_env_entries_then_add_row() {
        let mut env = BTreeMap::new();
        env.insert("ALPHA".to_string(), "1".to_string());
        env.insert("ZED".to_string(), "2".to_string());
        let app = app_with_env(env);

        let rows = config_rows(&app);
        let pos = |row: ConfigRow| rows.iter().position(|r| *r == row);
        let e0 = pos(ConfigRow::EnvEntry(0)).expect("first env row");
        let e1 = pos(ConfigRow::EnvEntry(1)).expect("second env row");
        let add = pos(ConfigRow::EnvAdd).expect("add-env row");
        assert!(e0 < e1 && e1 < add, "sorted entries precede the add row");
        assert_eq!(
            *rows.last().unwrap(),
            ConfigRow::Delete,
            "delete stays last"
        );
    }

    /// `Disabled` lives in the account-actions group at the bottom (with
    /// `Login`/`DeleteCreds`/`Delete`), one severity notch above `Delete` —
    /// not right below `Name` as a top toggle anymore. Locks the row builder
    /// order so a future edit can't silently drag it back to the top.
    #[test]
    fn disable_row_sits_in_the_account_actions_group_not_at_the_top() {
        let app = app_with_env(BTreeMap::new()); // OAuth, no stored creds, no custom env

        let rows = config_rows(&app);
        assert_eq!(
            rows[1],
            ConfigRow::AutoStart,
            "auto-start, not disabled, sits right after name"
        );
        assert!(
            !rows.contains(&ConfigRow::DeleteCreds),
            "sanity check for this fixture: no stored credential yet"
        );
        let pos = |row: ConfigRow| rows.iter().position(|r| *r == row);
        let login = pos(ConfigRow::Login).expect("login row always present");
        let disabled = pos(ConfigRow::Disabled).expect("disable row always present");
        let delete = pos(ConfigRow::Delete).expect("delete row always present");
        assert!(
            login < disabled,
            "disable sits in the tail account-actions group, after login"
        );
        assert!(disabled < delete, "disable is one notch above delete");
        assert_eq!(
            *rows.last().unwrap(),
            ConfigRow::Delete,
            "delete stays the very last row"
        );
    }

    fn app_with_profile(profile: Profile) -> App {
        App::new(AppConfig {
            state: AppState::default(),
            profiles: vec![profile],
        })
    }

    #[test]
    fn oauth_account_hides_api_key_keeps_auto_start() {
        let app = app_with_env(BTreeMap::new()); // no base url → OAuth
        let rows = config_rows(&app);
        assert!(
            !rows.contains(&ConfigRow::ApiKey),
            "api key is meaningless without a base url"
        );
        assert!(
            rows.contains(&ConfigRow::AutoStart),
            "auto-start is the OAuth-only row"
        );
    }

    #[test]
    fn api_account_shows_api_key_drops_auto_start() {
        let app = app_with_profile(Profile::new(
            "acct".to_string(),
            Some("https://api.test".to_string()),
            Some("sk-test".to_string()),
        ));
        let rows = config_rows(&app);
        assert!(
            rows.contains(&ConfigRow::ApiKey),
            "api key shows in API mode"
        );
        assert!(
            !rows.contains(&ConfigRow::AutoStart),
            "auto-start does not apply to API accounts"
        );
    }

    #[test]
    fn unset_overrides_collapse_behind_reveal_chip() {
        let app = app_with_env(BTreeMap::new());
        let rows = config_rows(&app);
        assert!(
            rows.contains(&ConfigRow::ModelOverrideAdd),
            "the reveal chip stands in for the unset overrides"
        );
        for row in [
            ConfigRow::OpusModel,
            ConfigRow::SonnetModel,
            ConfigRow::HaikuModel,
            ConfigRow::SubagentModel,
        ] {
            assert!(
                !rows.contains(&row),
                "unset override is hidden while collapsed"
            );
        }
    }

    #[test]
    fn set_override_renders_others_stay_collapsed() {
        let mut profile = Profile::new("acct".to_string(), None, None);
        profile.models.opus = Some("claude-opus-4-8".to_string());
        let rows = config_rows(&app_with_profile(profile));
        assert!(
            rows.contains(&ConfigRow::OpusModel),
            "a set override always renders"
        );
        assert!(
            !rows.contains(&ConfigRow::SonnetModel),
            "an unset sibling stays hidden"
        );
        assert!(
            rows.contains(&ConfigRow::ModelOverrideAdd),
            "the chip remains while any override is still unset"
        );
    }

    #[test]
    fn reveal_chip_expands_all_overrides() {
        let mut app = app_with_env(BTreeMap::new());
        enter_detail(&mut app);
        let chip = config_rows(&app)
            .iter()
            .position(|r| *r == ConfigRow::ModelOverrideAdd)
            .expect("reveal chip present while collapsed");
        app.config_action_cursor = chip;
        super::super::run_config_row(&mut app, ConfigRow::ModelOverrideAdd);
        assert!(
            app.config_draft
                .as_ref()
                .is_some_and(|d| d.overrides_expanded),
            "⏎ on the chip expands the override block"
        );
        let rows = config_rows(&app);
        for row in [
            ConfigRow::OpusModel,
            ConfigRow::SonnetModel,
            ConfigRow::HaikuModel,
            ConfigRow::SubagentModel,
        ] {
            assert!(rows.contains(&row), "every override shows once expanded");
        }
        assert!(
            !rows.contains(&ConfigRow::ModelOverrideAdd),
            "the chip is gone once expanded"
        );
    }

    #[test]
    fn add_field_with_managed_key_prompts_collision() {
        let _home = HomeSandbox::new();
        let mut app = app_with_env(BTreeMap::new());
        enter_detail(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.env_new_key = InputState::new("ANTHROPIC_BASE_URL");
            d.active = Some(ConfigRow::EnvAdd);
        }
        super::super::commit_env_new_key(&mut app);
        assert!(
            matches!(app.modals.last(), Some(Modal::EnvCollision(_))),
            "a clauth-managed key clash raises the collision prompt"
        );
    }

    #[test]
    fn add_field_with_fresh_key_inserts_and_edits_value() {
        let _home = HomeSandbox::new();
        let mut app = app_with_env(BTreeMap::new());
        enter_detail(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.env_new_key = InputState::new("CLAUDE_CODE_MAX_OUTPUT_TOKENS");
            d.active = Some(ConfigRow::EnvAdd);
        }
        super::super::commit_env_new_key(&mut app);

        assert!(app.modals.is_empty(), "a fresh key adds without prompting");
        assert_eq!(
            app.config()
                .find("acct")
                .and_then(|p| p.env.get("CLAUDE_CODE_MAX_OUTPUT_TOKENS").cloned()),
            Some(String::new()),
            "the key is added with an empty value"
        );
        assert!(
            matches!(
                app.config_draft.as_ref().and_then(|d| d.active),
                Some(ConfigRow::EnvEntry(_))
            ),
            "focus drops into the new entry's value editor"
        );
    }
}

/// The action menu's rotate/refresh gate is credential typing, not endpoint
/// routing: a hybrid holds a real token chain behind its `base_url` and must be
/// offered the rotate it can actually perform, while an endpoint-only account has
/// nothing to rotate.
#[test]
fn focused_account_types_the_hybrid_on_its_credential() {
    use super::focused_account;
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let mut hybrid = Profile::new(
        "hybrid".to_string(),
        Some("https://api.z.ai/api/anthropic".to_string()),
        None,
    );
    hybrid.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    let api_key_only = Profile::new(
        "apikey".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-test".to_string()),
    );

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![hybrid, api_key_only],
    });

    app.profile_cursor = 0;
    assert_eq!(
        focused_account(&app),
        Some(("hybrid".to_string(), true, true)),
        "a stored pair is rotatable no matter where requests route"
    );

    app.profile_cursor = 1;
    assert_eq!(
        focused_account(&app),
        Some(("apikey".to_string(), false, true)),
        "an endpoint-only account has no token chain"
    );
}

// ── banner wording ────────────────────────────────────────────────────────────
//
// "all accounts spent" needs evidence: a profile with a live spent window.
// A no-active state without one (e.g. a credential-less sole profile) gets
// the accurate "no active profile" wording instead (issue #2).

fn app_with_unlinked_profiles(profiles: Vec<crate::profile::Profile>) -> App {
    use crate::profile::{AppConfig, AppState};
    let names: Vec<_> = profiles.iter().map(|p| p.name.clone()).collect();
    App::new(AppConfig {
        state: AppState {
            profiles: names.clone(),
            fallback_chain: names,
            ..AppState::default()
        },
        profiles,
    })
}

#[test]
fn no_active_banner_without_spent_evidence() {
    use super::update_banner;
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a")]);
    update_banner(&mut app);
    assert_eq!(
        app.banner.as_ref().expect("banner").message,
        "no active account · select one to resume"
    );
}

#[test]
fn all_spent_banner_needs_live_spent_window() {
    use super::update_banner;
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mut spent = crate::testutil::blank_profile("a");
    spent.usage = Some(UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 100.0,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
        }),
        ..UsageInfo::default()
    });
    let mut app = app_with_unlinked_profiles(vec![spent]);
    update_banner(&mut app);
    assert_eq!(
        app.banner.as_ref().expect("banner").message,
        "all accounts spent · switch to an account to resume"
    );
}

/// A weekly window past the SOFT switch line but under the API's hard cap is not
/// evidence of a spent account: that member still serves requests, and `Off` (the
/// decision that clears the active in the first place) keys on the cap too.
#[test]
fn all_spent_banner_ignores_a_soft_blocked_member_that_still_serves() {
    use super::update_banner;
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mut soft = crate::testutil::blank_profile("a");
    soft.usage = Some(UsageInfo {
        seven_day: Some(UsageWindow {
            // Past the default 98 soft line, under the 100 hard cap.
            utilization: 99.0,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 86_400)),
        }),
        ..UsageInfo::default()
    });
    let mut app = app_with_unlinked_profiles(vec![soft]);
    update_banner(&mut app);
    assert_eq!(
        app.banner.as_ref().expect("banner").message,
        "no active account · select one to resume",
        "soft-blocked is not spent — the banner must not claim it is"
    );
}

/// A member weekly line UNDER the hard cap is a switch line, not death: 7d at
/// 95 with a `weekly at 90` override is past ITS line (the walk rotates off
/// it) but still serves. The banner keys on [`is_exhausted_hard`] — folding
/// the member line here would claim "all accounts spent" over a member that
/// answers requests fine. The fixture's override is what makes this test
/// discriminate: with no override the member line IS the hard cap, and the
/// folding revision passes it too.
#[test]
fn all_spent_banner_ignores_a_member_line_under_the_hard_cap() {
    use super::update_banner;
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mut overridden = crate::testutil::blank_profile("a");
    overridden.weekly_threshold = Some(90.0);
    overridden.usage = Some(UsageInfo {
        seven_day: Some(UsageWindow {
            utilization: 95.0,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 86_400)),
        }),
        ..UsageInfo::default()
    });
    let mut app = app_with_unlinked_profiles(vec![overridden]);
    update_banner(&mut app);
    assert_eq!(
        app.banner.as_ref().expect("banner").message,
        "no active account · select one to resume",
        "past the member line but under the cap still serves — not spent"
    );
}

/// The same member at the hard cap IS spent.
#[test]
fn all_spent_banner_fires_at_the_weekly_hard_cap() {
    use super::update_banner;
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mut dead = crate::testutil::blank_profile("a");
    dead.usage = Some(UsageInfo {
        seven_day: Some(UsageWindow {
            utilization: 100.0,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 86_400)),
        }),
        ..UsageInfo::default()
    });
    let mut app = app_with_unlinked_profiles(vec![dead]);
    update_banner(&mut app);
    assert_eq!(
        app.banner.as_ref().expect("banner").message,
        "all accounts spent · switch to an account to resume"
    );
}

// ── fallback continuous rows: `rotate at` + `weekly at` ──────────────────────
//
// Both are CONTINUOUS rows: unlike the enumerated Config-tab rows, they keep
// `+`/`-` for ±5 nudges alongside the `⏎` typed editor. `max spend` (a dollar
// ceiling) has no natural step unit and stays typed-only — see the no-op test
// below, next to the max-spend editor tests.

#[test]
fn fallback_threshold_plus_minus_still_nudge_both_ways() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile("a");
    profile.fallback_threshold = Some(50.0);
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = 0; // FALLBACK_ROWS[0] == Threshold

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.fallback_threshold),
        Some(55.0),
        "+ still raises the threshold by 5"
    );

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.fallback_threshold),
        Some(45.0),
        "- still lowers the threshold by 5"
    );
}

// `weekly at` joins `rotate at` as the second CONTINUOUS row (owner call,
// 2026-07-23): it now takes the same ±5 nudge alongside its existing `⏎`
// typed editor. `max spend` (a dollar ceiling) has no natural step unit and
// stays typed-only — see `fallback_max_spend_plus_minus_is_a_no_op` below.
#[test]
fn fallback_weekly_at_plus_minus_nudge_both_ways() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile("a");
    // 70 sits away from every default/bound (98 default, 50/100 bounds), so a
    // no-op bug (stays 70) and a mis-stepped nudge (anything but ±5) both
    // fail the exact-value assert below instead of accidentally landing on it.
    profile.weekly_threshold = Some(70.0);
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = weekly_at_row();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.weekly_threshold),
        Some(75.0),
        "+ raises the weekly override by exactly 5"
    );

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.weekly_threshold),
        Some(65.0),
        "- lowers the weekly override by exactly 5"
    );
}

#[test]
fn fallback_weekly_at_nudge_clamps_at_upper_bound() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile("a");
    profile.weekly_threshold = Some(99.0);
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = weekly_at_row();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.weekly_threshold),
        Some(100.0),
        "+ past MAX_WEEKLY_SWITCH_PCT clamps at 100"
    );
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.weekly_threshold),
        Some(100.0),
        "a further + stays pinned at the bound"
    );
}

#[test]
fn fallback_weekly_at_nudge_clamps_at_lower_bound() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile("a");
    profile.weekly_threshold = Some(51.0);
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = weekly_at_row();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.weekly_threshold),
        Some(50.0),
        "- past MIN_WEEKLY_SWITCH_PCT clamps at 50"
    );
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.weekly_threshold),
        Some(50.0),
        "a further - stays pinned at the bound"
    );
}

// Mirrors the dimmed-row contract `run_fallback_row`'s ⏎ already enforces:
// a gate-off weekly-at row isn't judged, so +/- must no-op exactly like ⏎
// does, instead of quietly arming an override nobody can see take effect.
#[test]
fn fallback_weekly_at_nudge_is_inert_while_gate_is_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile("a");
    profile.weekly_threshold = Some(70.0);
    profile.check_weekly = false;
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = weekly_at_row();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.weekly_threshold),
        Some(70.0),
        "+/- must no-op on a dimmed (gate-off) weekly-at row"
    );
}

// An unset override follows the chain-wide resolved line (rendered dimmed —
// see `detail_row`'s `weekly_default`); the first nudge must set an explicit
// override derived from THAT value, not from 0 or the hardcoded 98 default,
// so the on-screen number visibly moves by exactly 5 from what it showed.
#[test]
fn fallback_weekly_at_nudge_from_unset_bases_on_resolved_default() {
    let _home = crate::testutil::HomeSandbox::new();
    let profile = crate::testutil::blank_profile("a"); // weekly_threshold: None
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.config().state.weekly_switch_threshold = Some(80.0); // resolved chain default
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = weekly_at_row();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.weekly_threshold),
        Some(85.0),
        "first + on an unset override sets an explicit one at resolved-default + 5"
    );
}

// `space` still opens both the weekly-at and max-spend typed editors —
// unchanged by the `+`/`-` dispatch-by-row rewrite of `handle_fallback_detail_key`.
#[test]
fn fallback_weekly_at_and_max_spend_editors_still_open_via_space() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a")]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.config().state.spend_budget_switching = true; // arm max spend so its row isn't inert

    app.fallback_detail_cursor = weekly_at_row();
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        app.fallback_weekly_draft.is_some(),
        "space still opens the weekly-at editor"
    );
    super::handle_key(&mut app, key(KeyCode::Esc));

    app.fallback_detail_cursor = max_spend_row();
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        app.fallback_max_spend_draft.is_some(),
        "space still opens the max-spend editor"
    );
}

// ── fallback max auto-spend (real money) ────────────────────────────────────

/// Read the row's position rather than hardcoding it, so inserting a row above
/// it can't silently point these tests at a different field.
fn max_spend_row() -> usize {
    super::FALLBACK_ROWS
        .iter()
        .position(|r| *r == super::FallbackRow::MaxSpend)
        .expect("max spend row exists")
}

// `inf` and `nan` parse as perfectly good `f64`s, so a ceiling editor that only
// checked `>= 0.0` would accept "inf" and hand the chain an unbounded budget
// (`fallback::spend_room`). The typed editor is one of the two ways a ceiling
// reaches disk, so it refuses them at the keyboard, exactly like the config
// loader does for a hand-edited file.
#[test]
fn parse_max_spend_refuses_non_finite_and_negative() {
    assert_eq!(super::parse_max_spend("12.5"), Some(12.5));
    assert_eq!(super::parse_max_spend("0"), Some(0.0));
    assert_eq!(super::parse_max_spend("inf"), None);
    assert_eq!(super::parse_max_spend("-inf"), None);
    assert_eq!(super::parse_max_spend("NaN"), None);
    assert_eq!(super::parse_max_spend("-5"), None);
    assert_eq!(super::parse_max_spend("free"), None);
}

#[test]
fn fallback_max_spend_editor_types_and_persists() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a")]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = max_spend_row();
    // The ceiling is inert (editor won't open) until spend budget is armed.
    app.config().state.spend_budget_switching = true;
    assert_eq!(
        app.config().find("a").and_then(|p| p.max_auto_spend),
        None,
        "unset is the never-spend default"
    );

    // ⏎ opens the editor seeded with the current ceiling.
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert!(app.fallback_max_spend_draft.is_some(), "⏎ opens the field");

    // The field opens seeded with the current ceiling ("0.00"), so clear it
    // before typing or the digits append to it.
    for _ in 0..4 {
        super::handle_key(&mut app, key(KeyCode::Backspace));
    }
    for c in ['2', '5'] {
        super::handle_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_key(&mut app, key(KeyCode::Enter));
    assert!(app.fallback_max_spend_draft.is_none(), "⏎ closes the field");
    assert_eq!(
        app.config().find("a").and_then(|p| p.max_auto_spend),
        Some(25.0),
        "the typed ceiling persists"
    );
}

// The ceiling is inert (dimmed) while spend budget is off — a typed value would
// do nothing, so ⏎ must not open the editor.
#[test]
fn fallback_max_spend_editor_is_inert_while_spend_budget_is_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a")]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = max_spend_row();
    assert!(
        !app.config().state.spend_budget_switching,
        "spend budget off by default — the ceiling row is inert"
    );

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert!(
        app.fallback_max_spend_draft.is_none(),
        "⏎ must not open the editor while the row is inert"
    );
}

// A rejected value keeps the field open rather than toasting — the same
// no-toast treatment `rotate at` uses — so the inline invalid styling stays on
// screen until corrected, and nothing is written.
#[test]
fn fallback_max_spend_editor_refuses_an_infinite_ceiling() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a")]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = max_spend_row();
    app.config().state.spend_budget_switching = true;

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    // Seeded with "0.00"; clear it, then type the trap.
    for _ in 0..4 {
        super::handle_key(&mut app, key(KeyCode::Backspace));
    }
    for c in ['i', 'n', 'f'] {
        super::handle_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_key(&mut app, key(KeyCode::Enter));
    assert!(
        app.fallback_max_spend_draft.is_some(),
        "an invalid ceiling keeps the field open"
    );
    assert_eq!(
        app.config().find("a").and_then(|p| p.max_auto_spend),
        None,
        "an infinite ceiling must never reach disk"
    );
}

// A dollar ceiling has no natural step unit (unlike a bounded percent), so
// `max spend` stays typed-only — `+`/`-` fall through to the dispatcher's
// `_ => {}` arm. Armed (spend budget on) so a real nudge would be observable.
#[test]
fn fallback_max_spend_plus_minus_is_a_no_op() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile("a");
    profile.max_auto_spend = Some(10.0);
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = max_spend_row();
    app.config().state.spend_budget_switching = true;

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.max_auto_spend),
        Some(10.0),
        "max spend stays typed-only — +/- must never touch it"
    );
}

// ── fallback last-resort toggle (issue #8 follow-up) ─────────────────────────
//
// Space/⏎ on the `last resort` row flips `Profile::last_resort` and persists
// it, then kicks `refresh_tokens()` the same way `toggle_auto_start` does — a
// per-profile config.toml write doesn't bump `profiles.toml`'s mtime, so
// without the explicit kick the scheduler's cached token snapshot would lag
// until the next unrelated reload.

#[test]
fn fallback_last_resort_toggle_persists_and_refreshes_tokens() {
    use crate::profile::{ClaudeCredentials, OAuthToken};

    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile("a");
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-a".to_string(),
            refresh_token: Some("rt-a".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = 4; // FALLBACK_ROWS[4] == LastResort

    assert!(
        !app.config().find("a").is_some_and(|p| p.last_resort),
        "precondition: last_resort starts false"
    );

    // Simulate a stale token cache (the observable proof `refresh_tokens` ran):
    // App::new already populated it from `collect_tokens`, so clear it first.
    app.usage_tokens.lock().unwrap().clear();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    assert_eq!(
        app.config().find("a").map(|p| p.last_resort),
        Some(true),
        "space toggles last_resort on and persists it"
    );
    assert!(
        app.usage_tokens
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.name == "a"),
        "toggling last_resort must call refresh_tokens() to rebuild the token cache"
    );

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.config().find("a").map(|p| p.last_resort),
        Some(false),
        "⏎ toggles last_resort back off"
    );
}

// The chain has one parking spot: marking a member clears the mark everywhere
// else (radio), so two accounts can never both read `last resort ─●`.
#[test]
fn fallback_last_resort_is_exclusive_across_the_chain() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut b = crate::testutil::blank_profile("b");
    b.last_resort = true;
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a"), b]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0; // member "a"
    app.fallback_detail_cursor = 4; // FALLBACK_ROWS[4] == LastResort

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    assert_eq!(
        app.config().find("a").map(|p| p.last_resort),
        Some(true),
        "space marks the selected member"
    );
    assert_eq!(
        app.config().find("b").map(|p| p.last_resort),
        Some(false),
        "marking one member clears the previous last resort"
    );
    assert!(
        app.toasts.iter().any(|t| t.body.contains("moved from 'b'")),
        "the move away from the old member is surfaced"
    );

    // Turning the mark OFF touches nobody else.
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(app.config().find("a").map(|p| p.last_resort), Some(false));
    assert_eq!(app.config().find("b").map(|p| p.last_resort), Some(false));
}

// The per-account usage gates flip and persist through their toggle rows,
// independently of each other.
#[test]
fn fallback_usage_gate_toggles_persist() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a")]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;

    app.fallback_detail_cursor = 2; // FALLBACK_ROWS[2] == CheckWeekly
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config()
            .find("a")
            .map(|p| (p.check_weekly, p.check_scoped)),
        Some((false, true)),
        "space flips only the weekly gate"
    );

    app.fallback_detail_cursor = 3; // FALLBACK_ROWS[3] == CheckScoped
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.config()
            .find("a")
            .map(|p| (p.check_weekly, p.check_scoped)),
        Some((false, false)),
        "⏎ flips only the scoped gate"
    );

    // The off states survive a config reload from disk (persisted, not
    // just in-memory).
    let reloaded = crate::profile::load_profile("a").expect("reload profile");
    assert!(!reloaded.check_weekly);
    assert!(!reloaded.check_scoped);
}

// ── tokens tab: model filter via the action menu ─────────────────────────────

#[test]
fn tokens_action_menu_sets_and_swaps_the_model_filter() {
    use super::{ActionMenuAction, TokenFilter, build_action_menu, dispatch_action_menu_action};
    use crate::tokens::{ModelTokens, TokenStats};

    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Tokens;
    // Both models sit above OTHERS_THRESHOLD, so they group individually.
    app.token_stats = Some(TokenStats {
        models: vec![
            ModelTokens {
                model: "claude-opus-4-8".to_string(),
                input: 10_000_000,
                ..Default::default()
            },
            ModelTokens {
                model: "gpt-x".to_string(),
                input: 5_000_000,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    assert_eq!(super::token_model_count(&app), 2);

    // The menu offers the two inactive lenses plus the page-key mirrors.
    let labels: Vec<&str> = build_action_menu(&app)
        .items
        .iter()
        .map(|i| i.label)
        .collect();
    assert_eq!(
        labels,
        vec![
            "period: daily",
            "period: weekly",
            "period: monthly",
            "show claude models",
            "show other models",
            "toggle cache counting",
            "reload stats"
        ]
    );

    // Narrow to claude models; the cursor re-clamps into the shorter list.
    app.token_model_cursor = 1;
    dispatch_action_menu_action(&mut app, ActionMenuAction::TokensShowClaude);
    assert_eq!(app.token_filter, TokenFilter::Claude);
    assert_eq!(super::token_model_count(&app), 1);
    assert_eq!(
        app.token_model_cursor, 0,
        "cursor clamps into the filtered list"
    );

    // The active lens drops out of the menu; "show all" takes its place.
    let labels: Vec<&str> = build_action_menu(&app)
        .items
        .iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.contains(&"show all models"));
    assert!(!labels.contains(&"show claude models"));
}

// ── capture guard ─────────────────────────────────────────────────────────────

// An empty snapshot (no creds file, no endpoint config — the macOS-keychain
// state from issue #1) must refuse loudly instead of opening the name prompt
// and persisting a credential-less profile behind a success toast.
#[test]
fn capture_refuses_empty_snapshot() {
    use super::ToastKind;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    super::begin_capture(&mut app, false);
    assert!(app.modals.is_empty(), "no name prompt on an empty snapshot");
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Danger && t.body.contains("nothing to capture")),
        "danger toast names the problem"
    );
}

// ── capture-name collision (issue #7) ──────────────────────────────────────

/// Typing an EXISTING profile's name in the capture-name prompt must open the
/// confirm-overwrite modal instead of dead-ending with an "already exists"
/// error toast.
#[test]
fn capture_name_collision_opens_overwrite_confirm_instead_of_erroring() {
    use super::ToastKind;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("acme")]);

    let snapshot = crate::actions::CaptureSnapshot {
        credentials: None,
        base_url: Some("https://new.example.com".to_string()),
        api_key: Some("new-key".to_string()),
        identity: crate::actions::CaptureIdentity::LiveLogin,
    };
    app.modals
        .push(super::Modal::CaptureName(super::CaptureNameForm {
            snapshot: Box::new(snapshot),
            input: super::InputState::new("acme"),
            from_divergence: false,
        }));

    super::handle_capture_name_key(&mut app, key(KeyCode::Enter));

    assert!(
        app.toasts.iter().all(|t| t.kind != ToastKind::Danger),
        "typing an existing name must not dead-end with an error toast"
    );
    match app.modals.last() {
        Some(super::Modal::Confirm(state)) => {
            assert!(
                matches!(
                    &state.on_confirm,
                    super::ConfirmAction::CaptureOverwrite(_, name, false) if name.as_str() == "acme"
                ),
                "collision must route to CaptureOverwrite targeting the existing profile"
            );
        }
        other => panic!("expected a Confirm(CaptureOverwrite) modal, got {other:?}"),
    }
}

/// Cancelling the overwrite confirm must leave everything untouched: the
/// captured snapshot is dropped, config.toml/profiles.toml are byte-identical,
/// and the previously active profile stays active.
#[test]
fn capture_overwrite_cancel_changes_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut existing = crate::testutil::blank_profile("acme");
    existing.env.insert("FOO".to_string(), "bar".to_string());
    crate::profile::save_profile(&existing).expect("save existing");

    let mut app = app_with_unlinked_profiles(vec![existing]);
    app.config().state.active_profile = Some("acme".into());
    crate::profile::save_app_state(&app.config().state).expect("persist active profile");

    let config_toml = crate::profile::profile_dir("acme")
        .unwrap()
        .join("config.toml");
    let profiles_toml = crate::profile::clauth_dir().unwrap().join("profiles.toml");
    let before_config = std::fs::read(&config_toml).expect("read config.toml");
    let before_state = std::fs::read(&profiles_toml).expect("read profiles.toml");

    let snapshot = crate::actions::CaptureSnapshot {
        credentials: None,
        base_url: Some("https://new.example.com".to_string()),
        api_key: Some("new-key".to_string()),
        identity: crate::actions::CaptureIdentity::LiveLogin,
    };
    app.modals.push(super::Modal::Confirm(super::ConfirmState {
        message: "account 'acme' already exists.".to_string(),
        detail: None,
        choice: false, // cancel is the default-focused, safe choice
        on_confirm: super::ConfirmAction::CaptureOverwrite(
            Box::new(snapshot),
            "acme".to_string(),
            false,
        ),
    }));

    super::handle_confirm_key(&mut app, key(KeyCode::Enter));

    assert!(app.modals.is_empty(), "cancel dismisses the modal");
    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some("acme"),
        "active profile unchanged"
    );
    assert_eq!(
        std::fs::read(&config_toml).unwrap(),
        before_config,
        "config.toml byte-identical after cancel"
    );
    assert_eq!(
        std::fs::read(&profiles_toml).unwrap(),
        before_state,
        "profiles.toml byte-identical after cancel"
    );
}

// ── Setup tab: default-model row on the `+ new` create form (issue #12) ──────
//
// The row is the same hybrid alias-cycle field an existing account's model row
// is; the create form otherwise stays minimal (no alias overrides, no env).

mod new_account_model_row {
    use super::super::{
        App, ConfigFocus, ConfigRow, InputState, Tab, commit_new_account, config_rows, cycle_model,
        enter_config_detail,
    };
    use crate::profile::{AppConfig, AppState};
    use crate::testutil::HomeSandbox;

    fn empty_app() -> App {
        App::new(AppConfig {
            state: AppState::default(),
            profiles: Vec::new(),
        })
    }

    fn enter_new_account_form(app: &mut App) {
        app.tab = Tab::Setup;
        app.profile_cursor = app.profile_count(); // trailing "+ new" row
        enter_config_detail(app);
        assert_eq!(app.config_focus, ConfigFocus::Actions);
        assert_eq!(
            app.config_draft
                .as_ref()
                .and_then(|d| d.editing_name.clone()),
            None,
            "a fresh draft has no profile yet to persist into"
        );
    }

    #[test]
    fn create_form_carries_the_model_row_before_create() {
        let mut app = empty_app();
        enter_new_account_form(&mut app);
        let rows = config_rows(&app);
        let model_pos = rows
            .iter()
            .position(|r| *r == ConfigRow::Model)
            .expect("create form carries the base model row");
        let create_pos = rows
            .iter()
            .position(|r| *r == ConfigRow::Create)
            .expect("create row present");
        assert!(model_pos < create_pos, "model row precedes create");
        assert!(
            !rows.contains(&ConfigRow::OpusModel) && !rows.contains(&ConfigRow::ModelOverrideAdd),
            "the create form stays minimal: no alias overrides"
        );
    }

    #[test]
    fn space_cycles_the_draft_model_buffer_with_no_profile_to_persist_into() {
        let mut app = empty_app();
        enter_new_account_form(&mut app);

        for expected in ["opus", "sonnet", "haiku", "opusplan"] {
            cycle_model(&mut app);
            assert_eq!(app.config_draft.as_ref().unwrap().model.value, expected);
        }
        cycle_model(&mut app);
        assert_eq!(
            app.config_draft.as_ref().unwrap().model.value,
            "",
            "cycling past the last preset collapses back to unset `default`"
        );
    }

    #[test]
    fn create_persists_the_picked_model_to_the_new_profile() {
        let _home = HomeSandbox::new();
        let mut app = empty_app();
        enter_new_account_form(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.name = InputState::new("fresh");
        }
        cycle_model(&mut app); // "" -> "opus"

        commit_new_account(&mut app);

        assert_eq!(
            app.config()
                .find("fresh")
                .and_then(|p| p.models.default.clone()),
            Some("opus".to_string()),
            "the model picked on the create form persists to the new profile"
        );
    }

    #[test]
    fn create_persists_a_custom_model_id_too() {
        let _home = HomeSandbox::new();
        let mut app = empty_app();
        enter_new_account_form(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.name = InputState::new("fresh");
            // The ⏎ custom-id editor edits this same draft buffer in place.
            d.model = InputState::new("claude-fable-5");
        }

        commit_new_account(&mut app);

        assert_eq!(
            app.config()
                .find("fresh")
                .and_then(|p| p.models.default.clone()),
            Some("claude-fable-5".to_string()),
            "a typed custom id persists through create, not just presets"
        );
    }

    #[test]
    fn create_without_touching_model_leaves_it_unset() {
        let _home = HomeSandbox::new();
        let mut app = empty_app();
        enter_new_account_form(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.name = InputState::new("bare");
        }

        commit_new_account(&mut app);

        assert_eq!(
            app.config()
                .find("bare")
                .and_then(|p| p.models.default.clone()),
            None,
            "default stays unset on purpose, matching default claude code behaviour"
        );
    }
}

// ── AUTH-1 gate on the TUI switch (Incident C, every entry point) ───────────

/// An OAuth profile with stored credentials on disk, so a passed gate can
/// complete the relink. `expires_at` picks the gate branch: far future reads
/// as healthy, past as expiring (routes through the injected refresher).
fn stored_oauth_profile(name: &str, expires_at: i64) -> crate::profile::Profile {
    use crate::profile::{ClaudeCredentials, OAuthToken, save_profile};
    let mut p = crate::testutil::blank_profile(name);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(expires_at),
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&p).expect("save profile");
    p
}

fn far_future() -> i64 {
    crate::usage::now_ms() as i64 + 3_600_000
}

fn already_expired() -> i64 {
    crate::usage::now_ms() as i64 - 60_000
}

/// `collect_tokens` snapshots the persisted quarantine flag so the scheduler's
/// partition can widen a flagged profile's cadence without a config lock.
#[test]
fn collect_tokens_carries_the_auth_broken_flag() {
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken};
    let creds = |name: &str| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };
    let mut flagged = crate::testutil::blank_profile("flagged");
    flagged.credentials = Some(creds("flagged"));
    let mut clean = crate::testutil::blank_profile("clean");
    clean.credentials = Some(creds("clean"));

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![flagged, clean],
    };
    config.set_auth_broken("flagged", true);

    let entries = super::collect_tokens(&config);
    let get = |n: &str| entries.iter().find(|e| e.name == n).expect("entry");
    assert!(get("flagged").auth_broken, "flag rides the snapshot");
    assert!(!get("clean").auth_broken, "unflagged stays clear");
}

/// A dead login whose flag hasn't been set yet must still be refused: the
/// switch runs the full `ensure_installable` gate (off the UI thread in
/// production), not the flag-only check that let an unflagged dead token
/// into the Keychain.
#[test]
fn tui_switch_gate_refuses_a_dead_target_before_its_flag_is_set() {
    use super::ToastKind;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![stored_oauth_profile("dead", already_expired())]);
    assert!(!app.config().is_auth_broken("dead"), "flag starts clear");

    super::spawn_switch_gate(&mut app, "dead".to_string(), |_, _| {
        Err(crate::oauth::RefreshError::Invalid("revoked".to_string()))
    });
    super::drain_switch_gates(&mut app);

    assert!(
        !app.config().is_active("dead"),
        "a dead target must never become active"
    );
    assert!(
        app.config().is_auth_broken("dead"),
        "the gate quarantines the dead login"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Danger && t.body.contains("clauth login dead")),
        "the refusal names the recovery"
    );
}

/// The healthy path stays a plain switch: a target with real token life never
/// touches the refresher and lands active once the gate answer drains.
#[test]
fn tui_switch_gate_passes_a_healthy_target_through() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![stored_oauth_profile("healthy", far_future())]);

    super::spawn_switch_gate(&mut app, "healthy".to_string(), |_, _| {
        panic!("a healthy target must not spend a refresh")
    });
    super::drain_switch_gates(&mut app);

    assert!(app.config().is_active("healthy"), "healthy target switches");
    assert!(
        crate::usage::is_idle(&app.activity, "healthy"),
        "the pending mark clears once the gate answers"
    );
}

/// A transient gate failure (network, busy rotation lock) refuses the switch
/// without quarantining — retry is free, a false flag is not.
#[test]
fn tui_switch_gate_transient_failure_refuses_without_quarantine() {
    use super::ToastKind;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app =
        app_with_unlinked_profiles(vec![stored_oauth_profile("flaky", already_expired())]);

    super::spawn_switch_gate(&mut app, "flaky".to_string(), |_, _| {
        Err(crate::oauth::RefreshError::Transient(anyhow::anyhow!(
            "no network"
        )))
    });
    super::drain_switch_gates(&mut app);

    assert!(!app.config().is_active("flaky"), "refused this attempt");
    assert!(
        !app.config().is_auth_broken("flaky"),
        "a network blip must not quarantine"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Danger && t.body.contains("could not refresh 'flaky'")),
        "the refusal says retry, not re-login"
    );
}

/// A flagged target whose chain actually recovered switches after the gate
/// refreshes it — the same self-heal the CLI/MCP gates already had, where the
/// old flag-only check refused until some other site lifted the flag.
#[test]
fn tui_switch_gate_recovers_a_flagged_target() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![stored_oauth_profile("flagged", far_future())]);
    app.config().set_auth_broken("flagged", true);

    super::spawn_switch_gate(&mut app, "flagged".to_string(), |_, _| {
        Ok(crate::oauth::TokenResponse {
            access_token: "at-recovered".to_string(),
            refresh_token: "rt-recovered".to_string(),
            expires_in: 3600,
            scope: None,
        })
    });
    super::drain_switch_gates(&mut app);

    assert!(
        app.config().is_active("flagged"),
        "a recovered chain switches"
    );
    assert!(
        !app.config().is_auth_broken("flagged"),
        "the successful refresh lifts the flag"
    );
}

/// The gate answer is the pending switch's only completion path: it waits out
/// open modals (completion can raise the Divergence prompt, which must not
/// stack) and blocks a second switch while in flight (a later switch landing
/// first would be overturned by the older gate's answer).
#[test]
fn tui_switch_gate_pending_blocks_switches_and_waits_for_modals() {
    use super::{ConfirmAction, Modal, ToastKind};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![
        stored_oauth_profile("first", far_future()),
        stored_oauth_profile("second", far_future()),
    ]);

    super::spawn_switch_gate(&mut app, "first".to_string(), |_, _| {
        panic!("healthy target: no refresh")
    });
    // Un-drained gate = switch in flight: a second switch is refused.
    super::run_confirm_action(&mut app, ConfirmAction::Switch("second".to_string()));
    assert!(
        !app.config().is_active("second"),
        "a second switch mid-gate is refused"
    );
    assert!(
        app.toasts.iter().any(|t| t.kind == ToastKind::Warning),
        "the refusal is surfaced"
    );

    // An open modal defers completion to a later tick.
    app.modals.push(Modal::Help);
    super::drain_switch_gates(&mut app);
    assert!(
        !app.config().is_active("first"),
        "no completion under an open modal"
    );
    app.modals.pop();
    super::drain_switch_gates(&mut app);
    assert!(
        app.config().is_active("first"),
        "completion lands once the modal closes"
    );
}

/// A quarantined target stays refused end to end through `perform_switch`
/// (the production entry): the flagged blank profile has no refresh token, so
/// the gate confirms `Broken` without HTTP and the drain surfaces the login
/// hint.
#[test]
fn tui_switch_refuses_a_quarantined_target_with_login_hint() {
    use super::ToastKind;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile("healthy"),
        crate::testutil::blank_profile("broken"),
    ]);
    app.config().set_auth_broken("broken", true);

    super::perform_switch(&mut app, "broken");
    super::drain_switch_gates(&mut app);

    assert!(
        !app.config().is_active("broken"),
        "a quarantined target must never become active"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Danger && t.body.contains("clauth login broken")),
        "the refusal names the recovery"
    );
}

#[test]
fn tokens_period_key_cycles_and_clamps_cursor() {
    use super::{KeyCode, Tab, TokenPeriod, TokenView, handle_key};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Tokens;
    // Two lifetime rows; the daily/scoped lists are empty (no `today`, no
    // per-day models), so cycling must clamp the Models cursor.
    app.token_stats = Some(crate::tokens::TokenStats {
        models: vec![
            crate::tokens::ModelTokens {
                model: "claude-opus-4".into(),
                input: 10,
                output: 5,
                cache_read: 0,
                cache_create: 0,
            },
            crate::tokens::ModelTokens {
                model: "claude-sonnet-4".into(),
                input: 8,
                output: 4,
                cache_read: 0,
                cache_create: 0,
            },
        ],
        ..Default::default()
    });
    app.token_view = TokenView::Models;
    app.token_model_cursor = 1;

    for expected in [
        TokenPeriod::Daily,
        TokenPeriod::Weekly,
        TokenPeriod::Monthly,
        TokenPeriod::Lifetime,
    ] {
        handle_key(&mut app, crate::testutil::key(KeyCode::Char('t')));
        assert_eq!(app.token_period, expected, "t cycles in declared order");
    }
    // The first hop landed on the empty daily list, so the cursor was clamped
    // to 0 and stays there through the full cycle.
    assert_eq!(app.token_model_cursor, 0, "cursor clamps on an empty lens");
}

// ── tokens tab: loading-spinner busy flag ─────────────────────────────────────

/// `tokens_topping_up` drives the tab's loading spinners. Only a seeding `Base`
/// (first paint) or a manual reload lights it; `Loaded`/`Failed` clear it, and a
/// silent periodic `Base` (stats already present) must stay dark.
#[test]
fn tokens_topping_up_tracks_the_load_lifecycle() {
    use super::{drain_tokens_events, reload_token_stats};
    use crate::profile::{AppConfig, AppState};
    use crate::tokens::{TokenStats, TokensEvent};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    // App::new drops the loader's sender under cfg(test); rewire a live channel
    // so the test can feed the loader's events.
    let (tx, rx) = std::sync::mpsc::channel();
    app.tokens_events = rx;

    assert!(app.token_stats.is_none());
    assert!(!app.tokens_topping_up);

    // First (seeding) Base: paints the cache and marks the top-up in flight.
    tx.send(TokensEvent::Base(Box::<TokenStats>::default()))
        .unwrap();
    drain_tokens_events(&mut app);
    assert!(app.token_stats.is_some(), "seeding Base paints the tab");
    assert!(
        app.tokens_topping_up,
        "a seeding Base lights the loading flag"
    );

    // Sweep progress lands in `tokens_progress` while the top-up runs.
    tx.send(TokensEvent::Progress {
        done: 25,
        total: 380,
    })
    .unwrap();
    drain_tokens_events(&mut app);
    assert_eq!(
        app.tokens_progress,
        Some((25, 380)),
        "Progress stores the sweep counts"
    );

    // Loaded clears both the flag and the counts.
    tx.send(TokensEvent::Loaded(Box::<TokenStats>::default()))
        .unwrap();
    drain_tokens_events(&mut app);
    assert!(!app.tokens_topping_up, "Loaded clears the loading flag");
    assert_eq!(app.tokens_progress, None, "Loaded clears the sweep counts");

    // A silent periodic Base (stats already present) must NOT relight it.
    tx.send(TokensEvent::Base(Box::<TokenStats>::default()))
        .unwrap();
    drain_tokens_events(&mut app);
    assert!(
        !app.tokens_topping_up,
        "a non-seeding periodic Base stays silent"
    );

    // Manual reload lights it (and drops any stale counts); a subsequent
    // Failed clears both.
    app.tokens_progress = Some((1, 2));
    reload_token_stats(&mut app);
    assert!(app.tokens_topping_up, "manual reload lights the flag");
    assert_eq!(
        app.tokens_progress, None,
        "manual reload drops stale sweep counts"
    );
    tx.send(TokensEvent::Failed).unwrap();
    drain_tokens_events(&mut app);
    assert!(!app.tokens_topping_up, "Failed clears the loading flag");
    assert_eq!(app.tokens_progress, None, "Failed clears the sweep counts");
}

/// A quarantined login must be VISIBLE without a switch attempt: the row's
/// fetch state shows the dead login's 429/cached mask, so the one system
/// banner names it (danger) — outranking the divergence banner, yielding to
/// the no-active danger. Clears the moment the flag lifts.
#[test]
fn auth_broken_member_raises_the_system_banner() {
    use super::{BannerSeverity, update_banner};
    use crate::profile::{AppConfig, AppState, Profile, save_profile};
    let _home = crate::testutil::HomeSandbox::new();

    let work = Profile::new("work".to_string(), None, None);
    save_profile(&work).expect("save work");
    let broken = Profile::new("dead".to_string(), None, None);
    save_profile(&broken).expect("save dead");
    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["work".into(), "dead".into()],
            active_profile: Some("work".into()),
            ..AppState::default()
        },
        profiles: vec![work, broken],
    };
    config.set_auth_broken("dead", true);
    let mut app = App::new(config);

    update_banner(&mut app);
    let banner = app
        .banner
        .clone()
        .expect("a broken member raises the banner");
    assert_eq!(banner.severity, BannerSeverity::Danger);
    assert!(
        banner.message.contains("clauth login dead"),
        "the banner names the recovery: {}",
        banner.message
    );

    app.config().set_auth_broken("dead", false);
    update_banner(&mut app);
    assert!(
        app.banner.is_none(),
        "the banner clears with the flag: {:?}",
        app.banner
    );
}

// CDX-1 T8: `perform_switch` dispatches a codex target to the codex path —
// the codex slot flips, the claude slot never moves, and the OAuth switch
// gate is never consulted (local file work only).
#[test]
fn tui_switch_dispatches_codex_targets_to_the_codex_slot() {
    use super::ToastKind;
    let _home = crate::testutil::HomeSandbox::new();

    fn auth(access: &str, acct: &str) -> Vec<u8> {
        serde_json::json!({
            "tokens": {
                "access_token": access,
                "refresh_token": format!("rt-{access}"),
                "account_id": acct,
            },
        })
        .to_string()
        .into_bytes()
    }
    let mut cdx_a = crate::testutil::blank_profile("cdx-a");
    cdx_a.harness = crate::profile::Harness::Codex;
    let mut cdx_b = crate::testutil::blank_profile("cdx-b");
    cdx_b.harness = crate::profile::Harness::Codex;
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile("claude-a"),
        cdx_a,
        cdx_b,
    ]);
    {
        let mut cfg = app.config();
        cfg.state.active_profile = Some("claude-a".into());
        cfg.state.active_codex_profile = Some("cdx-b".into());
        cfg.state.profiles = vec!["claude-a".into(), "cdx-a".into(), "cdx-b".into()];
        // Persist: the switch's wholesale state re-sync reloads from disk
        // (disk is the cross-process truth), so unpersisted in-memory state
        // would be — correctly — dropped.
        crate::profile::save_app_state(&cfg.state).unwrap();
    }
    crate::codex::write_profile_auth("cdx-a", &auth("at-a", "acct-a")).unwrap();
    crate::codex::write_profile_auth("cdx-b", &auth("at-b", "acct-b")).unwrap();
    crate::codex::write_live(&auth("at-b", "acct-b")).unwrap();

    super::perform_switch(&mut app, "cdx-a");

    let cfg = app.config();
    assert_eq!(cfg.state.active_codex_profile.as_deref(), Some("cdx-a"));
    assert_eq!(
        cfg.state.active_profile.as_deref(),
        Some("claude-a"),
        "the claude slot must never move on a codex switch"
    );
    drop(cfg);
    let live = crate::codex::read_live().unwrap().expect("live");
    assert!(String::from_utf8_lossy(&live).contains("at-a"));
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Success && t.body.contains("codex now uses 'cdx-a'")),
        "the switch is surfaced"
    );
}

// CDX-1/2: a codex profile's Setup rows carry NO claude-shaped settings —
// no endpoint (edit_profile_endpoint refuses codex targets; the UI must not
// offer the dead end), no auto-start, no model overrides, no env. It keeps
// Name / Login (re-capture) / DeleteCreds (with a stored login) / Delete.
#[test]
fn config_rows_for_a_codex_profile_hide_claude_settings() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut cdx = crate::testutil::blank_profile("cdx-a");
    cdx.harness = crate::profile::Harness::Codex;
    let mut app = super::App::new(crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![cdx],
    });
    app.config_draft = None;
    app.profile_cursor = 0;

    let rows = super::config_rows(&app);
    for banned in [
        super::ConfigRow::BaseUrl,
        super::ConfigRow::ApiKey,
        super::ConfigRow::AutoStart,
        super::ConfigRow::Model,
        super::ConfigRow::ModelOverrideAdd,
        super::ConfigRow::EnvAdd,
    ] {
        assert!(!rows.contains(&banned), "codex must not offer {banned:?}");
    }
    assert!(
        rows.contains(&super::ConfigRow::Login),
        "re-capture stays offered"
    );
    assert!(
        !rows.contains(&super::ConfigRow::DeleteCreds),
        "no stored login yet → no log-out row"
    );

    crate::codex::write_profile_auth(
        "cdx-a",
        br#"{"tokens":{"access_token":"at-a","account_id":"acct-a"}}"#,
    )
    .unwrap();
    let rows = super::config_rows(&app);
    assert!(
        rows.contains(&super::ConfigRow::DeleteCreds),
        "a stored codex login makes log-out reachable"
    );
}

/// FALLBACK_ROWS index of the `weekly at` override editor.
fn weekly_at_row() -> usize {
    super::FALLBACK_ROWS
        .iter()
        .position(|r| *r == super::FallbackRow::WeeklyAt)
        .expect("WeeklyAt row exists")
}

// The per-member weekly override: ⏎ opens seeded with the current override
// (empty when following the chain default), a typed value persists, and an
// EMPTY commit clears back to the default. Inert while the weekly gate is off.
#[test]
fn fallback_weekly_override_editor_sets_and_clears() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a")]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = weekly_at_row();

    // ⏎ opens the editor with an EMPTY seed (no override yet).
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert!(app.fallback_weekly_draft.is_some(), "⏎ opens the field");
    for c in ['9', '0'] {
        super::handle_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_key(&mut app, key(KeyCode::Enter));
    assert!(app.fallback_weekly_draft.is_none(), "⏎ closes the field");
    assert_eq!(
        app.config().find("a").and_then(|p| p.weekly_threshold),
        Some(90.0),
        "the typed override persists"
    );

    // Re-open: seeded with "90"; clear it and commit EMPTY → back to default.
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    for _ in 0..2 {
        super::handle_key(&mut app, key(KeyCode::Backspace));
    }
    super::handle_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.config().find("a").and_then(|p| p.weekly_threshold),
        None,
        "an empty commit clears the override"
    );
}

// The per-account usage gates flip and persist through their toggle rows,
// independently of each other; the override row is inert while the weekly
// gate is off.
#[test]
fn fallback_usage_gate_toggles_persist_and_gate_off_inerts_the_override() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a")]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;

    app.fallback_detail_cursor = 2; // FALLBACK_ROWS[2] == CheckWeekly
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config()
            .find("a")
            .map(|p| (p.check_weekly, p.check_scoped)),
        Some((false, true)),
        "space flips only the weekly gate"
    );

    app.fallback_detail_cursor = 3; // FALLBACK_ROWS[3] == CheckScoped
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.config()
            .find("a")
            .map(|p| (p.check_weekly, p.check_scoped)),
        Some((false, false)),
        "⏎ flips only the scoped gate"
    );

    // Weekly gate is now off → the override editor must not open.
    app.fallback_detail_cursor = weekly_at_row();
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert!(
        app.fallback_weekly_draft.is_none(),
        "⏎ must not open the editor while the weekly gate is off"
    );

    // The off states survive a config reload from disk (persisted, not
    // just in-memory).
    let reloaded = crate::profile::load_profile("a").expect("reload profile");
    assert!(!reloaded.check_weekly);
    assert!(!reloaded.check_scoped);
}

// ── apply_usage Fresh-gate (docs/todo.md #1) ─────────────────────────────────
//
// `App::apply_usage` is driven every tick over the shared usage stores. The
// bell + the burn-rate history JSONL append must fire ONLY when the per-
// profile status is `FetchStatus::Fresh`. A phantom entry from a
// `RateLimited` or stale-`Cached` tick survives restart and skews the burn
// rate; a false bell cries wolf. The three tests below inject the status
// directly into `usage_status` — the same field the scheduler writes on
// every fetch — then call `apply_usage` and assert the durable side effects.
//
// The seam: `apply_usage` reads each profile's status out of the shared
// `usage_status` map (`Arc<RankedMutex<HashMap<String, FetchStatus>>>`), so
// seeding that map from a test is indistinguishable from a real scheduler
// tick landing a fetch result.

use crate::usage::{FetchStatus, UsageInfo, UsageWindow};

/// Single-profile fixture: "alice" with `bell_threshold = 70.0` and a seeded
/// `usage_store["alice"]` at 80 % utilization (>= threshold, so the bell
/// would fire if the gate were removed). The injected `status` lands in
/// `usage_status["alice"]`.
const GATE_PROFILE: &str = "alice";
const GATE_THRESHOLD: f64 = 70.0;
const GATE_UTIL: f64 = 80.0;

/// Pre-seed `usage_history.jsonl` with one entry at 50 % utilization so the
/// Fresh case has something to differ from (forcing `changed = true`), and
/// the RateLimited/Cached cases can assert byte-identical no-op. Returns the
/// file's bytes after seeding (one line, util 50).
fn seed_prior_history_entry() -> String {
    let path = crate::profile::profile_history_path(GATE_PROFILE)
        .expect("profile_history_path resolves under the sandbox home");
    std::fs::create_dir_all(path.parent().expect("parent dir"))
        .expect("create profile dir for seeded history");
    let old = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 50.0,
            resets_at: None,
        }),
        ..UsageInfo::default()
    };
    let usage_json = serde_json::to_string(&old).unwrap_or_default();
    let name_json = serde_json::to_string(GATE_PROFILE).unwrap_or_default();
    let line = format!(
        r#"{{"ts":{},"name":{},"usage":{}}}"#,
        crate::usage::now_ms().saturating_sub(60_000),
        name_json,
        usage_json,
    );
    std::fs::write(&path, format!("{line}\n")).expect("seed prior history entry");
    std::fs::read_to_string(&path).expect("read seeded history")
}

/// Build a fresh `App` over the caller-held sandbox. Caller owns the
/// `HomeSandbox` so it outlives the App's disk writes.
fn gate_app(
    _home: &crate::testutil::HomeSandbox,
    status: FetchStatus,
) -> (App, std::path::PathBuf) {
    let mut profile = crate::testutil::blank_profile(GATE_PROFILE);
    profile.bell_threshold = Some(GATE_THRESHOLD);
    let app = app_with(vec![profile]);
    let usage = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: GATE_UTIL,
            resets_at: None,
        }),
        ..UsageInfo::default()
    };
    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    {
        let mut store = app.usage_store.lock().expect("usage_store mutex poisoned");
        store.insert(GATE_PROFILE.to_string(), usage);
    }
    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    {
        let mut s = app
            .usage_status
            .lock()
            .expect("usage_status mutex poisoned");
        s.insert(GATE_PROFILE.to_string(), status);
    }
    let history_path = crate::profile::profile_history_path(GATE_PROFILE)
        .expect("profile_history_path resolves under the sandbox home");
    (app, history_path)
}

#[test]
fn apply_usage_fresh_status_fires_bell_and_appends_history() {
    let _home = crate::testutil::HomeSandbox::new();
    let prior = seed_prior_history_entry();
    let (mut app, history_path) = gate_app(&_home, FetchStatus::Fresh);

    app.apply_usage();

    // Bell arm: util(80) >= threshold(70), no prior bell_fired entry → fires.
    assert_eq!(
        app.bell_fired.get(GATE_PROFILE),
        Some(&true),
        "Fresh + util over threshold must ring the bell",
    );

    // History arm: file gains a new entry carrying the live util.
    let after =
        std::fs::read_to_string(&history_path).expect("history file readable after apply_usage");
    assert!(
        after.len() > prior.len(),
        "Fresh must append a new history line (pre {} bytes, post {} bytes)",
        prior.len(),
        after.len(),
    );
    assert!(
        after.contains(r#""utilization":"#.to_string().as_str())
            && after.contains(&format!("{}", GATE_UTIL)),
        "the appended live sample must carry the seeded utilization {GATE_UTIL} (got: {after})",
    );
    assert!(
        after.starts_with(&prior),
        "the prior entry must survive intact at the head of the file",
    );
}

#[test]
fn apply_usage_rate_limited_status_skips_bell_and_history_append() {
    let _home = crate::testutil::HomeSandbox::new();
    let prior = seed_prior_history_entry();
    let (mut app, history_path) = gate_app(&_home, FetchStatus::RateLimited);

    app.apply_usage();

    assert!(
        !app.bell_fired.contains_key(GATE_PROFILE),
        "RateLimited must not ring the bell (util would have fired on Fresh)",
    );
    let after = std::fs::read_to_string(&history_path).expect("history file still readable");
    assert_eq!(
        after, prior,
        "RateLimited must not append a phantom history entry (file must be byte-identical)",
    );
}

#[test]
fn apply_usage_cached_status_skips_bell_and_history_append() {
    let _home = crate::testutil::HomeSandbox::new();
    let prior = seed_prior_history_entry();
    let (mut app, history_path) = gate_app(&_home, FetchStatus::Cached);

    app.apply_usage();

    assert!(
        !app.bell_fired.contains_key(GATE_PROFILE),
        "Cached must not ring the bell (util would have fired on Fresh)",
    );
    let after = std::fs::read_to_string(&history_path).expect("history file still readable");
    assert_eq!(
        after, prior,
        "Cached must not append a phantom history entry (file must be byte-identical)",
    );
}

// ── finish_bootstrap's Fresh-only auto-switch gate ───────────────────────────
//
// The startup switch one-shot is a switch DECISION taken off numbers nobody
// re-verified this run. A Cached / RateLimited / Failed read is unverified in
// either direction, so acting on it can relink live credentials over a window
// the account no longer has; those profiles are due on the scheduler's first
// tick, which fetches first and decides off the corrected numbers.
//
// The seam: `usage_store` + `usage_status` are exactly what the bootstrap
// worker fills before it posts `StartupSignal::BootstrapDone`, and
// `finish_bootstrap` reads the gate off `apply_usage`'s copy of them — so
// seeding the maps and sending the signal is indistinguishable from a real
// bootstrap landing.

const BOOT_SPENT: &str = "spent";
const BOOT_SPARE: &str = "spare";

/// 5h window at `utilization` with a reset an hour out — the exhaustion
/// predicates only trust a window they can prove live.
fn boot_window(utilization: f64) -> UsageWindow {
    UsageWindow {
        utilization,
        resets_at: Some(crate::usage::epoch_secs_to_iso(
            crate::usage::now_epoch_secs() + 3600,
        )),
    }
}

/// Drive one bootstrap tail over the caller-held sandbox with `status` as the
/// ACTIVE profile's last read. Everything else — chain, windows, credentials,
/// the spare's own Fresh status — is identical across calls, so `status` is the
/// only variable between the two directions.
fn bootstrap_app(_home: &crate::testutil::HomeSandbox, status: FetchStatus) -> App {
    use super::{StartupSignal, drain_startup_signals};
    use crate::profile::{AppConfig, AppState, Profile, save_profile};

    let mk = |name: &str| {
        let mut p = Profile::new(name.to_string(), None, None);
        p.credentials = Some(creds_ra(&format!("rt-{name}"), &format!("at-{name}")));
        save_profile(&p).expect("save profile");
        p
    };
    let spent = mk(BOOT_SPENT);
    let spare = mk(BOOT_SPARE);
    // The live file is the ACTIVE account's captured mirror: the relink has no
    // uncaptured login to strand, so a decided switch actually lands.
    write_live_creds(spent.credentials.as_ref().expect("spent credentials"));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some(BOOT_SPENT.into()),
            profiles: vec![BOOT_SPENT.into(), BOOT_SPARE.into()],
            fallback_chain: vec![BOOT_SPENT.into(), BOOT_SPARE.into()],
            ..AppState::default()
        },
        profiles: vec![spent, spare],
    });

    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    {
        let mut store = app.usage_store.lock().expect("usage_store mutex poisoned");
        store.insert(
            BOOT_SPENT.to_string(),
            UsageInfo {
                five_hour: Some(boot_window(100.0)),
                ..UsageInfo::default()
            },
        );
        store.insert(
            BOOT_SPARE.to_string(),
            UsageInfo {
                five_hour: Some(boot_window(1.0)),
                ..UsageInfo::default()
            },
        );
    }
    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    {
        let mut s = app
            .usage_status
            .lock()
            .expect("usage_status mutex poisoned");
        s.insert(BOOT_SPENT.to_string(), status);
        s.insert(BOOT_SPARE.to_string(), FetchStatus::Fresh);
    }

    // `finish_bootstrap` starts the real scheduler via `spawn_refresher`, which
    // now skips spawning the tick thread under `cfg!(test)` (its kick-block
    // seed already ran synchronously, on this thread, before that check) — so
    // no tick ever runs and nothing can resolve home past this sandbox. The
    // flag store below is now belt-and-suspenders: the one-shot under test
    // never reads it.
    app.shutting_down.store(true, Ordering::SeqCst);

    app.startup_sender
        .send(StartupSignal::BootstrapDone)
        .expect("send bootstrap signal");
    drain_startup_signals(&mut app);
    app
}

fn toast_bodies(app: &App) -> Vec<String> {
    app.toasts.iter().map(|t| t.body.clone()).collect()
}

/// Every `FetchStatus` the gate can see. The skip case iterates this filtered by
/// [`skips_the_one_shot`] rather than restating its own list, so there is ONE
/// place to grow when a variant is added. Growing it is comment-enforced, not
/// compile-enforced — an array length can't be tied to a variant count without a
/// derive crate — but the match below fails to compile first, which lands
/// whoever adds a variant here.
const ALL_STATUSES: [FetchStatus; 4] = [
    FetchStatus::Fresh,
    FetchStatus::Cached,
    FetchStatus::RateLimited,
    FetchStatus::Failed,
];

/// Exhaustiveness tripwire over `FetchStatus`. The gate keys on `== Fresh`, so
/// every variant added later is non-Fresh and must be driven through the skip
/// case. An unhandled variant fails THIS match to compile, one line from the
/// [`ALL_STATUSES`] entry it also needs.
fn skips_the_one_shot(status: FetchStatus) -> bool {
    match status {
        FetchStatus::Fresh => false,
        FetchStatus::Cached | FetchStatus::RateLimited | FetchStatus::Failed => true,
    }
}

#[test]
fn bootstrap_one_shot_switches_off_a_fresh_exhausted_active() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = bootstrap_app(&_home, FetchStatus::Fresh);

    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some(BOOT_SPARE),
        "a Fresh read of a maxed active must land the startup switch",
    );
    assert_eq!(
        toast_bodies(&app),
        vec!["auto-switched to 'spare'".to_string()],
        "the landed switch announces its target",
    );
}

#[test]
fn bootstrap_one_shot_skips_a_non_fresh_active_read() {
    let skipped: Vec<FetchStatus> = ALL_STATUSES
        .into_iter()
        .filter(|s| skips_the_one_shot(*s))
        .collect();
    // A derived list can go EMPTY and pass vacuously, so pin its size: everything
    // but `Fresh` has to reach the loop below.
    assert_eq!(
        skipped.len(),
        ALL_STATUSES.len() - 1,
        "every non-Fresh variant must be driven through the gate, got {skipped:?}",
    );

    for status in skipped {
        let _home = crate::testutil::HomeSandbox::new();
        let app = bootstrap_app(&_home, status);

        assert_eq!(
            app.config().state.active_profile.as_deref(),
            Some(BOOT_SPENT),
            "{status:?} numbers are unverified — the active must stay put for the first tick",
        );
        assert_eq!(
            toast_bodies(&app),
            Vec::<String>::new(),
            "{status:?} must decide nothing, so it announces nothing",
        );
    }
}

// The override row is inert while the member's weekly gate is off — the line
// isn't judged there, so ⏎ must not open the editor.
#[test]
fn fallback_weekly_override_editor_is_inert_while_gate_is_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut a = crate::testutil::blank_profile("a");
    a.check_weekly = false;
    let mut app = app_with_unlinked_profiles(vec![a]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = weekly_at_row();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert!(
        app.fallback_weekly_draft.is_none(),
        "⏎ must not open the editor while the weekly gate is off"
    );
}

// ── startup Overwrite + logged-out shell: caller-path coverage for the sink ──
//
// `force_snapshot_skips_shell_but_still_captures_real_divergence` (claude.rs)
// pins the sink guard in isolation. This drives the REAL startup chain end to
// end: reconcile_startup classifies the shell as diverged and posts
// ReconcileNeedsPrompt; draining the signal runs resolve_or_note_divergence →
// default_divergence Overwrite → run_divergence_choice →
// force_snapshot_active_credentials (the shared sink). The 1Hz poll bails on
// `live_credentials_are_shell()` BEFORE that point (app.rs), but the startup
// path has no such early guard, so the sink's empty-login skip is the only
// thing standing between a logged-out shell and the stored login here.

fn oauth_login(access: &str, refresh: Option<&str>) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

#[test]
fn startup_overwrite_default_routes_a_shell_through_the_guarded_sink() {
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, DivergenceChoice, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    // Active profile carrying a real stored login.
    let mut profile = Profile::new("active".to_string(), None, None);
    profile.credentials = Some(oauth_login("stored-access", Some("stored-refresh")));
    crate::profile::save_profile(&profile).expect("save profile");

    // CC's logged-out shell in the live slot: blank tokens, a foreign key kept,
    // written as a plain file (not clauth's symlink).
    let live = crate::profile::claude_dir()
        .expect("claude dir")
        .join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(
        &live,
        r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0},"mcpOAuth":{}}"#,
    )
    .expect("write shell");

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.active_profile = Some("active".into());
    config.state.profiles = vec!["active".into()];
    config.state.default_divergence = Some(DivergenceChoice::Overwrite);

    let mut app = App::new(config);

    // Exactly how production drives startup: reconcile inline, then drain the
    // signal it posts.
    super::reconcile_startup(&mut app);
    super::drain_startup_signals(&mut app);

    // The sink's empty-login skip held: the stored login is intact, not blanked
    // by the shell. Remove that guard and Overwrite writes the shell's blank
    // tokens over the stored chain here, so this assertion reds.
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir("active")
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read stored");
    assert_eq!(
        stored.access_token(),
        Some("stored-access"),
        "a startup Overwrite must not let a logged-out shell blank the stored access token",
    );
    assert_eq!(
        stored.refresh_token(),
        Some("stored-refresh"),
        "a startup Overwrite must not let a logged-out shell blank the stored refresh token",
    );

    // Positive control: the Overwrite branch actually RAN (the shell reached the
    // sink, not an earlier bail). It relinked the live slot back to the stored
    // login, so the slot no longer holds the shell's blank token. Cross-platform:
    // a symlink on unix, a copy on windows both read back the stored login.
    let relinked: ClaudeCredentials =
        crate::profile::read_json_file(&live).expect("read relinked live");
    assert_eq!(
        relinked.access_token(),
        Some("stored-access"),
        "Overwrite relinks the live slot to the stored login, replacing the shell",
    );
}
