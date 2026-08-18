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

/// The delegates pane's rows arrive BANDED, from the store.
///
/// `recompute_plugin_checks` is the pane's only reader, and it must call
/// `jobs::list_banded` — the same function `clauth jobs` and `monitor`'s listing
/// call — rather than `jobs::list`. The renderer sorts nothing any more, so this
/// read is the whole of the pane's ordering: reverting it to the raw retention
/// order silently drops a long-running delegate below a burst of completions,
/// which is the defect the shared function exists to prevent, and no render test
/// would catch it because they all feed their fixtures in directly.
///
/// Cross-band on purpose, and the finished rows are anchored NEWER than the live
/// one, so raw retention order and banded order disagree.
#[test]
fn the_delegates_pane_reads_the_store_in_banded_order() {
    use crate::mcp::jobs::{self, JobPhase};

    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    let live = |job_id: &str, anchor_ago: u64| {
        jobs::write_heartbeat(
            &jobs::RunningSpec {
                job_id: job_id.to_string(),
                profile: "acct".to_string(),
                started_at: now - 900_000,
                recorded_at: now - 900_000,
                timeout_secs: 0,
                endpoint: None,
                idle_secs: Some(300),
                kind: jobs::RecordKind::Collectable,
            },
            now - anchor_ago,
            "working",
        )
        .expect("heartbeat");
    };
    let done = |job_id: &str, anchor_ago: u64| {
        let dir = jobs::jobs_dir().expect("jobs dir");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join(format!("{job_id}.json")),
            serde_json::to_vec(&serde_json::json!({
                "job_id": job_id,
                "profile": "acct",
                "state": "done",
                "started_at": now - 900_000,
                "done_at": now - anchor_ago,
                "envelope": { "result": "ok" },
            }))
            .expect("serialize"),
        )
        .expect("write");
    };

    // INTERLEAVED by anchor, live and finished alternating. A fixture where the
    // live rows are all oldest (or all newest) is banded correctly by any
    // monotone reordering of the anchor, so a mutant that merely reverses the
    // order passes it — the "the mutant is non-equivalent and the fixture cannot
    // tell" shape. Here no reordering of the anchor produces the banded answer.
    live("d-live-new-0", 1_000);
    done("d-fin-a-0", 2_000);
    live("d-live-old-0", 3_000);
    done("d-fin-b-0", 4_000);

    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);

    let ids: Vec<String> = app
        .plugin
        .delegates
        .iter()
        .map(|j| j.record.job_id.clone())
        .collect();
    let phases: Vec<JobPhase> = app.plugin.delegates.iter().map(|j| j.phase()).collect();

    assert_eq!(phases.len(), 4, "fixture control: every record was read");
    assert_eq!(
        phases,
        vec![
            JobPhase::Running,
            JobPhase::Running,
            JobPhase::Done,
            JobPhase::Done
        ],
        "live band whole and first, finished band after it: {ids:?}"
    );
    // And within each band the store's own newest-mattering order survives, so
    // the banding is the ONLY thing the read reordered.
    assert_eq!(
        ids,
        vec!["d-live-new-0", "d-live-old-0", "d-fin-a-0", "d-fin-b-0"],
        "each band still newest-mattering first",
    );
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
fn plugin_check_warns_and_offers_global_install_when_project_local() {
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
    // The old shell copy-paste hint is gone; the row now offers the one-key
    // user-scope install fix instead.
    assert!(
        check.fix.is_some(),
        "non-global install should offer the install fix, got {:?}",
        check.detail
    );
    assert!(
        check.detail.iter().any(|line| line.starts_with("[f]")),
        "the detail should show the install fix hint, got {:?}",
        check.detail
    );
}

#[test]
fn plugin_check_offers_install_fix_when_missing() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);
    let check = plugin_check(&app);
    assert_eq!(check.health, super::Health::Warn);
    assert!(
        check
            .detail
            .iter()
            .any(|line| line.starts_with("installed: no")),
        "an absent plugin should read not installed, got {:?}",
        check.detail
    );
    assert!(
        check.fix.is_some(),
        "a missing plugin should offer the install fix, got {:?}",
        check.detail
    );
    assert!(
        check.detail.iter().any(|line| line.starts_with("[f]")),
        "the detail should show the install fix hint, got {:?}",
        check.detail
    );
}

// ── the install fix drives agentgear at user scope ────────────────────────

/// The confirm gate every mutating fix owes (tab spec: "confirm modal first,
/// default choice = cancel"): `f` on the plugin row must open a
/// [`ConfirmAction::InstallPlugin`] modal that defaults to cancel and runs
/// nothing until confirmed.
#[test]
fn the_install_fix_opens_a_default_cancel_confirm_before_installing() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);
    let idx = app
        .plugin
        .checks
        .iter()
        .position(|c| c.label == "plugin")
        .expect("plugin check present");
    app.plugin.cursor = idx;

    super::apply_plugin_fix(&mut app);

    let Some(super::Modal::Confirm(state)) = app.modals.last() else {
        panic!(
            "the install fix must open a confirm modal, got {:?}",
            app.modals.last()
        );
    };
    assert!(!state.choice, "the install confirm must default to cancel");
    assert!(
        matches!(state.on_confirm, super::ConfirmAction::InstallPlugin),
        "the modal must run the install on confirm"
    );
    assert!(app.toasts.is_empty(), "arming the fix must run nothing yet");
    assert!(
        crate::plugin_probe::installed_records().is_empty(),
        "nothing may be installed before the confirm"
    );
}

/// The pin the whole install fix hangs on: `ConfirmAction::InstallPlugin` runs
/// agentgear's `install(Scope::User, Source::Embedded)` — visible here as the
/// exact `claude plugin` invocations the fake CLI records (user scope on both
/// the marketplace add and the install, exactly one install per confirm), the
/// materialized tree landing under the hermetic data dir, and the Plugin tab
/// recomputing to `installed`.
#[cfg(unix)]
#[test]
fn the_install_fix_runs_agentgear_user_scope_install() {
    let home = crate::testutil::HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = crate::testutil::ConfigDirSandbox::new(&home, &config);
    let fake = crate::testutil::FakeClaude::new(&home);

    let mut app = bare_app();
    super::run_confirm_action(&mut app, super::ConfirmAction::InstallPlugin);

    let log = fake.log();
    assert!(
        log.lines().any(|l| l == "--version"),
        "agentgear gates the install on the CLI version floor, got:\n{log}"
    );
    let add = log
        .lines()
        .find(|l| l.starts_with("plugin marketplace add "))
        .expect("the marketplace must be added");
    assert!(
        add.ends_with("--scope user"),
        "the marketplace add must target user scope, got:\n{log}"
    );
    let tree_arg = add
        .trim_start_matches("plugin marketplace add ")
        .trim_end_matches(" --scope user");
    // Derive the expectation from the same resolution agentgear uses: on linux
    // `dirs::data_dir()` honors the XDG_DATA_HOME pin, on macOS it derives from
    // `$HOME` (pinned by the harness) and ignores XDG_DATA_HOME, so a
    // hardcoded data dir could only hold on one platform.
    let data_dir = dirs::data_dir().expect("the data dir resolves under the pins");
    assert!(
        data_dir.starts_with(home.home()),
        "the data dir must resolve inside the sandbox, got: {}",
        data_dir.display()
    );
    assert!(
        std::path::Path::new(tree_arg).starts_with(data_dir.join("clauth")),
        "the marketplace source must be the materialized tree under the \
         hermetic data dir, got: {tree_arg}"
    );
    let installs = log
        .lines()
        .filter(|l| *l == "plugin install clauth@clauth --scope user")
        .count();
    assert_eq!(
        installs, 1,
        "exactly one install at user scope per confirmed fix, got:\n{log}"
    );
    assert!(
        log.lines().any(|l| l == "plugin list --json"),
        "agentgear must verify the install through the registry, got:\n{log}"
    );

    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == super::ToastKind::Success && t.body.contains("installed")),
        "the confirmed install toasts success, got: {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );
    let check = plugin_check(&app);
    assert_eq!(
        check.health,
        super::Health::Ok,
        "the recomputed row reads installed and healthy, got {:?}",
        check.detail
    );
    assert!(
        check.detail.iter().any(|l| l.starts_with("installed: yes")),
        "the row reflects the user-scope install, got {:?}",
        check.detail
    );
}

/// A no-op install from a row that read "not installed" means the backend
/// never ran (`claude` absent). The toast must say so as a warning, never a
/// green success over a skipped install, and the row stays not-installed.
#[cfg(unix)]
#[test]
fn the_install_fix_warns_when_claude_is_missing() {
    let home = crate::testutil::HomeSandbox::new();
    let config = home.home().join(".claude-config");
    std::fs::create_dir_all(&config).expect("config dir");
    let _config = crate::testutil::ConfigDirSandbox::new(&home, &config);
    let _fake = crate::testutil::FakeClaude::new_without_claude(&home);

    let mut app = bare_app();
    super::run_confirm_action(&mut app, super::ConfirmAction::InstallPlugin);

    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == super::ToastKind::Warning && t.body.contains("no changes")),
        "a skipped install toasts a warning naming the no-op, got: {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );
    assert!(
        !app.toasts
            .iter()
            .any(|t| t.kind == super::ToastKind::Success),
        "no success toast over a skipped install, got: {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );
    let check = plugin_check(&app);
    assert_eq!(
        check.health,
        super::Health::Warn,
        "the row must stay not-installed, got {:?}",
        check.detail
    );
    assert!(
        check.detail.iter().any(|l| l.starts_with("installed: no")),
        "the row still reads not installed, got {:?}",
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
    assert!(check.detail.iter().any(|l| l == "accounts: 1"));
    // A zero count prints NOTHING — no row, and specifically not `live: 0`
    // or a `—` placeholder. The Overview cell and the Fallback card both hide
    // their zero; this row was the one surface still announcing it.
    assert!(
        !check.detail.iter().any(|l| l.starts_with("live:")),
        "an idle fleet says nothing about sessions, got {:?}",
        check.detail
    );
    assert!(check.detail.iter().any(|l| l == "active: \u{2014}"));
    assert!(check.detail.iter().any(|l| l == "link: \u{2014}"));
}

/// A session that swapped A→B holds BOTH accounts' liveness markers: B's
/// because that is what it authenticates as, A's because the chain the child
/// still holds in memory must not rotate underneath it. Summing per-profile
/// marker
/// counts therefore reports one child as two sessions and names an account
/// nothing authenticates as — A is the wrong answer, not a changed one. Only the
/// registry can tell the two apart.
///
/// Driven through `r` (`refresh_version`), the one path that re-collects the
/// fleet tally instead of folding in whatever the tick last left in
/// `app.live_sessions` — so this also pins that `r` re-reads the registry.
#[test]
fn runtime_check_counts_a_swapped_session_once_on_the_member_it_moved_to() {
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![
            Profile::new("swap-a".to_string(), None, None),
            Profile::new("swap-b".to_string(), None, None),
        ],
    });

    let sid = "4242-0";
    let mut row = crate::testutil::live_row(sid, "swap-a");
    row.current_member = Some("swap-b".to_string());
    row.last_swap_at = Some(1_700_000_060_000);
    crate::live_sessions::register(&row).expect("register row");
    // Both markers, exactly as a swapped session holds them.
    let _launch = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("swap-a"),
        false,
        sid,
    )
    .expect("hold the launch member's marker");
    let _landed = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("swap-b"),
        false,
        sid,
    )
    .expect("hold the swapped-onto member's marker");

    super::recompute_plugin_checks(&mut app, true);

    let check = app
        .plugin
        .checks
        .iter()
        .find(|c| c.label == "runtime")
        .expect("runtime check");
    assert_eq!(
        check
            .detail
            .iter()
            .find(|l| l.starts_with("live:"))
            .map(String::as_str),
        Some("live: 1 across 1 account"),
        "one child is one session on one account, got {:?}",
        check.detail
    );
    assert!(
        check.detail.iter().any(|l| l == "  swap-b"),
        "the account the session moved ONTO is the one it runs as, got {:?}",
        check.detail
    );
    assert!(
        !check.detail.iter().any(|l| l == "  swap-a"),
        "nothing authenticates as the launch member any more, got {:?}",
        check.detail
    );
}

/// The runtime row folds in the tally the tick already collected, never a sweep
/// of its own: `LiveTally::collect` is two readdirs plus an `open` + `try_lock`
/// per row plus a credential read, and the render thread ran it once a second
/// for an answer `poll_live_sessions` had put in `app.live_sessions` the same
/// tick. Two independent derivations of one number can also disagree inside a
/// frame, which no amount of caching fixes.
///
/// The seeded fleet is one the registry does NOT hold, so a recompute that
/// collects again reports an empty fleet and reds here.
#[test]
fn the_runtime_check_reads_the_ticks_tally_rather_than_sweeping_again() {
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![Profile::new("acct".to_string(), None, None)],
    });
    app.live_sessions =
        crate::live_sessions::LiveTally::of([crate::testutil::live_row("4242-0", "acct")]);

    super::recompute_plugin_checks(&mut app, false);

    let check = app
        .plugin
        .checks
        .iter()
        .find(|c| c.label == "runtime")
        .expect("runtime check");
    assert_eq!(
        check
            .detail
            .iter()
            .find(|l| l.starts_with("live:"))
            .map(String::as_str),
        Some("live: 1 across 1 account"),
        "the row renders the tick's tally, got {:?}",
        check.detail
    );
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
        let p = cfg
            .find(&crate::profile::ProfileName::from("api"))
            .expect("profile present");
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
    let mut draft = build_draft_existing(&app, &crate::profile::ProfileName::from("oauth"));
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
    let p = cfg
        .find(&crate::profile::ProfileName::from("hybrid"))
        .expect("profile present");
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
    let draft = build_draft_existing(&app, &crate::profile::ProfileName::from("hybrid"));
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
    let p = cfg
        .find(&crate::profile::ProfileName::from("api"))
        .expect("profile present");
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
    let p = cfg
        .find(&crate::profile::ProfileName::from("oauth"))
        .expect("profile present");
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

    perform_delete(&mut app, &crate::profile::ProfileName::from("busy"));
    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("busy"))
            .is_some(),
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
        app.config()
            .find(&crate::profile::ProfileName::from("busy"))
            .is_none(),
        "confirming deletes the profile despite the live session"
    );
}

/// The rename commit takes its rotation guard OUTSIDE the config lock, the way
/// the delete path above does. ROTATION ranks outside `Config`, so acquiring it
/// under `app.config()` is the inversion `lockorder`'s `debug_assert!` fires on
/// — and until this test existed nothing drove `commit_rename` at all, so that
/// comment was enforced by nothing: inverting the two lines reddened zero of
/// 2563 tests.
///
/// Ordering only. It asserts the rename landed, and the lock-order assert is
/// what makes an inverted acquisition a panic rather than a pass.
#[test]
fn committing_a_rename_takes_its_guard_outside_the_config_lock() {
    use super::{ConfigRow, build_draft_existing, commit_config_field};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("before".to_string(), None, None)]);
    app.profile_cursor = 0;
    let mut draft = build_draft_existing(&app, &crate::profile::ProfileName::from("before"));
    draft.name = super::InputState::new("after");
    app.config_draft = Some(draft);

    commit_config_field(&mut app, ConfigRow::Name);

    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("after"))
            .is_some()
            && app
                .config()
                .find(&crate::profile::ProfileName::from("before"))
                .is_none(),
        "the rename must land under the account's new name"
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

    perform_delete(&mut app, &crate::profile::ProfileName::from("quiet"));

    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("quiet"))
            .is_none(),
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

    toggle_profile_disabled(&mut app, &crate::profile::ProfileName::from("acct"));
    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .is_disabled(),
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

    toggle_profile_disabled(&mut app, &crate::profile::ProfileName::from("acct"));
    let p = app
        .config()
        .find(&crate::profile::ProfileName::from("acct"))
        .cloned()
        .unwrap();
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
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("acct"),
    ));

    run_config_row(&mut app, ConfigRow::Disabled);

    assert!(
        !app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .is_disabled(),
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
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("acct"),
    ));

    run_config_row(&mut app, ConfigRow::Disabled);

    assert!(
        !app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .is_disabled(),
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
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("acct"),
    ));

    run_config_row(&mut app, ConfigRow::Disabled);
    assert!(
        !app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .is_disabled(),
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
        app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .is_disabled(),
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
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("acct"),
    ));

    run_config_row(&mut app, ConfigRow::Disabled);

    assert!(
        !app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .is_disabled(),
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

// ── `clear long-lived token` row (CLA-SPLIT escape hatch) ──────────────────

/// A credential blob for the CLA-SPLIT fixtures below.
fn split_creds(access: &str, refresh: Option<&str>) -> crate::profile::ClaudeCredentials {
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

/// Write `name`'s `session-token.json` by hand, the way the CLA-SPLIT fill
/// does. `refresh: None` is a genuine long-lived mint; `Some(..)` is the
/// mis-filled shape (a rotating pair) the split disengages for.
fn seed_session_token(name: &str, refresh: Option<&str>) {
    let dir =
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("profile dir");
    std::fs::create_dir_all(&dir).expect("mkdir profile");
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&split_creds("mint-access", refresh)).expect("serialize sidecar"),
    )
    .expect("write sidecar");
}

/// The escape hatch exists only while there is something to escape from: no
/// `session-token.json`, no row. A MIS-FILLED sidecar gets the row too — the
/// split is disengaged there, but the operator still believes it is armed and
/// deleting the file by hand was the only exit. Gating on `has_session_token`
/// (long-lived only) would hide the row in exactly that state.
#[test]
fn config_rows_clear_session_token_row_tracks_the_sidecar() {
    use super::{ConfigRow, config_rows};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("acct".to_string(), None, None);
    acct.credentials = Some(split_creds("stored-oauth", Some("stored-refresh")));
    crate::profile::save_profile(&acct).expect("save profile");
    let mut app = app_with(vec![acct]);
    app.profile_cursor = 0;
    app.config_draft = None;

    assert!(
        !config_rows(&app).contains(&ConfigRow::ClearSessionToken),
        "no sidecar → nothing to clear, so no row"
    );

    seed_session_token("acct", None);
    assert!(
        config_rows(&app).contains(&ConfigRow::ClearSessionToken),
        "a long-lived sidecar shows the row"
    );

    seed_session_token("acct", Some("rotating"));
    assert!(
        config_rows(&app).contains(&ConfigRow::ClearSessionToken),
        "a mis-filled sidecar needs the same exit"
    );

    // The widened arms, each alone. A set flag with nothing stamped is the
    // state where hiding the row leaves the daemon to re-create what the
    // operator has no surface left to remove; a preserved mint alone is a
    // year-scale credential only this row owns.
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("acct"))
        .expect("profile dir");
    std::fs::remove_file(dir.join("session-token.json")).expect("drop the sidecar");
    {
        let mut cfg = app.config();
        cfg.find_mut(&crate::profile::ProfileName::from("acct"))
            .expect("acct")
            .rolling_token = true;
    }
    assert!(
        config_rows(&app).contains(&ConfigRow::ClearSessionToken),
        "a set flag alone keeps the row"
    );
    {
        let mut cfg = app.config();
        cfg.find_mut(&crate::profile::ProfileName::from("acct"))
            .expect("acct")
            .rolling_token = false;
    }
    assert!(
        !config_rows(&app).contains(&ConfigRow::ClearSessionToken),
        "flag down, nothing stored — the row goes again"
    );
    std::fs::write(dir.join("session-token.static.json"), b"{}").expect("backup");
    assert!(
        config_rows(&app).contains(&ConfigRow::ClearSessionToken),
        "a preserved mint alone keeps the row"
    );
}

/// The row refuses on a profile storing no OTHER login — the same refusal
/// `cmd_static_token_clear` makes on the CLI, since clearing there would
/// strip the profile's only credential. Inert means inert: pressing twice
/// neither arms nor clears nor toasts.
#[test]
fn clear_session_token_row_is_inert_without_another_stored_login() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let acct = Profile::new("acct".to_string(), None, None);
    crate::profile::save_profile(&acct).expect("save profile");
    seed_session_token("acct", None);
    let sidecar = crate::profile::profile_dir(&crate::profile::ProfileName::from("acct"))
        .expect("profile dir")
        .join("session-token.json");

    let mut app = app_with(vec![acct]);
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("acct"),
    ));

    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    run_config_row(&mut app, ConfigRow::ClearSessionToken);

    assert!(
        sidecar.exists(),
        "a gated row must not clear the profile's only credential"
    );
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.armed_action),
        None,
        "a gated row must not even arm"
    );
    assert!(
        app.toasts.is_empty(),
        "a gated row stays silent, like the `disabled` row's own gate"
    );
}

/// Arm-then-confirm, and the relink that makes the confirm safe. On the ACTIVE
/// Make `name` the active account the way production does, in memory AND on
/// disk. Every write of `active_profile` in the tree persists in the same
/// breath (`actions::finish_switch`, and both TUI sites), so an in-memory-only
/// fixture is a state no code path can produce, and it stops driving the arm
/// under test the moment a reader goes to disk for it.
fn make_active(app: &mut App, name: &str) {
    let mut cfg = app.config();
    cfg.state.active_profile = Some(name.into());
    crate::profile::save_app_state(&cfg.state).expect("persist the active account");
}

/// profile the live `~/.claude/.credentials.json` is a symlink INTO the store
/// pointed at `session-token.json`, so removing that target without relinking
/// leaves a dangling link and Claude Code reads no credentials at all — the
/// `read_link` assertion is what pins the relink, not the toast.
#[cfg(unix)]
#[test]
fn clear_session_token_arms_then_clears_and_relinks_the_active_account() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("acct".to_string(), None, None);
    acct.credentials = Some(split_creds("stored-oauth", Some("stored-refresh")));
    crate::profile::save_profile(&acct).expect("save profile");
    seed_session_token("acct", None);
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("acct"))
        .expect("profile dir");
    let sidecar = dir.join("session-token.json");
    // Where a switch on a split profile leaves the live slot: symlinked AT the
    // sidecar, which is precisely what makes a bare delete dangle.
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("acct"))
        .expect("link");
    let live = home.home().join(".claude").join(".credentials.json");
    assert_eq!(
        std::fs::read_link(&live).expect("live is a symlink"),
        sidecar,
        "fixture: the live slot starts on the sidecar"
    );

    let mut app = app_with(vec![acct]);
    make_active(&mut app, "acct");
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("acct"),
    ));

    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    assert!(sidecar.exists(), "the first press must only arm the row");
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.armed_action),
        Some(ConfigRow::ClearSessionToken),
        "the first press arms this row"
    );
    assert!(app.toasts.is_empty(), "arming alone must not toast");

    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    assert!(
        !sidecar.exists(),
        "the second press, while armed, clears the sidecar"
    );
    assert_eq!(
        std::fs::read_link(&live).expect("live is still a symlink"),
        dir.join("credentials.json"),
        "the active account's live link must land on its stored OAuth login, not dangle"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.body.contains("cleared") && t.body.contains("relinked its own login")),
        "the confirmed clear toasts what it actually did, got {:?}",
        app.toasts
    );
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.armed_action),
        None,
        "the arm must clear after firing — this row disappears with the sidecar, \
         but a stale arm would still leak onto whichever row takes its index"
    );
}

/// The same row on an ACTIVE API-KEY account. The gate passes on EITHER stored
/// credential, so this clears fine — onto an ABSENT install source, where the
/// forcing relink removes the live slot and (on macOS) signs the Keychain out
/// instead of installing anything. The toast read "relinked its own login" here
/// until 2026-08-12, which is the opposite of what happened, and the CLI's own
/// line said the same. This is the leg that separates the two outcomes; the
/// OAuth leg above cannot, since both branches relink something there.
#[test]
fn clear_session_token_on_an_active_api_key_account_reports_the_sign_out() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    // An api key and NO OAuth pair: clearable, with nothing to fall back to.
    let mut acct = Profile::new("acct".to_string(), None, None);
    acct.api_key = Some("sk-ant-api-key".to_string());
    crate::profile::save_profile(&acct).expect("save profile");
    seed_session_token("acct", None);
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("acct"))
        .expect("profile dir");
    let sidecar = dir.join("session-token.json");
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("acct"))
        .expect("link");
    let live = home.home().join(".claude").join(".credentials.json");
    assert_eq!(
        std::fs::read_link(&live).expect("live is a symlink"),
        sidecar,
        "fixture: the live slot starts on the sidecar"
    );

    let mut app = app_with(vec![acct]);
    make_active(&mut app, "acct");
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("acct"),
    ));

    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    assert!(sidecar.exists(), "the first press must only arm the row");
    run_config_row(&mut app, ConfigRow::ClearSessionToken);

    assert!(
        !sidecar.exists(),
        "the second press, while armed, clears the sidecar"
    );
    assert!(
        live.symlink_metadata().is_err(),
        "with no store to relink onto, the live slot is removed rather than left dangling"
    );

    let body = app
        .toasts
        .iter()
        .map(|t| t.body.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        body.contains("signed Claude Code out") && body.contains("api key"),
        "the toast must name the sign-out and what the account runs on: {body}"
    );
    assert!(
        !body.contains("relinked"),
        "nothing was relinked, so nothing may say so: {body}"
    );
}

/// The relink is scoped to the ACTIVE profile. Clearing an idle profile's
/// sidecar must leave the live slot pointing where it already pointed —
/// relinking unconditionally would repoint `~/.claude/.credentials.json` at a
/// profile the user never switched to.
#[cfg(unix)]
#[test]
fn clear_session_token_on_an_idle_account_leaves_the_live_link_alone() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut live_acct = Profile::new("live".to_string(), None, None);
    live_acct.credentials = Some(split_creds("live-oauth", Some("live-refresh")));
    crate::profile::save_profile(&live_acct).expect("save live");
    let mut idle = Profile::new("idle".to_string(), None, None);
    idle.credentials = Some(split_creds("idle-oauth", Some("idle-refresh")));
    crate::profile::save_profile(&idle).expect("save idle");
    seed_session_token("idle", None);
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("live"))
        .expect("link live");

    let mut app = app_with(vec![live_acct, idle]);
    make_active(&mut app, "live");
    app.profile_cursor = 1;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("idle"),
    ));

    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    run_config_row(&mut app, ConfigRow::ClearSessionToken);

    assert!(
        !crate::profile::profile_dir(&crate::profile::ProfileName::from("idle"))
            .expect("profile dir")
            .join("session-token.json")
            .exists(),
        "the idle account's sidecar still clears"
    );
    let live_link = home.home().join(".claude").join(".credentials.json");
    assert_eq!(
        std::fs::read_link(&live_link).expect("live is a symlink"),
        crate::profile::profile_dir(&crate::profile::ProfileName::from("live"))
            .expect("profile dir")
            .join("credentials.json"),
        "clearing an idle account must not repoint the live slot at it"
    );
}

/// The TUI clear is the same FULL exit as `clauth static-token --clear`, or the
/// two surfaces fight the daemon differently: on a rolling profile the
/// `rolling_token` flag goes FIRST (a set flag has the daemon re-stamp a fresh
/// bearer over the removal on its next scan) and the preserved mint goes too
/// (a "cleared" long-lived token with a year-scale mint still in
/// `session-token.static.json` is not cleared).
#[test]
fn clear_session_token_on_a_rolling_profile_disarms_and_takes_the_backup() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("roll".to_string(), None, None);
    acct.credentials = Some(split_creds("stored-oauth", Some("stored-refresh")));
    acct.rolling_token = true;
    crate::profile::save_profile(&acct).expect("save profile");
    // A mint first, then the rolling stamp: the first stamp preserves the mint
    // into the backup slot — the exact two-file state a rolling profile
    // carries in production.
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from("roll"),
        "sk-ant-oat01-tui-clear-rolling-mint0",
        crate::usage::now_ms() as i64,
    )
    .expect("mint");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from("roll"),
        &crate::profile::OAuthToken {
            access_token: "at-rolled".to_string(),
            refresh_token: None,
            expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".into()),
        },
    )
    .expect("stamp");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("roll"))
        .expect("profile dir");
    assert!(
        dir.join("session-token.static.json").exists(),
        "fixture: the stamp preserved the mint"
    );

    let mut app = app_with(vec![acct]);
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("roll"),
    ));

    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    run_config_row(&mut app, ConfigRow::ClearSessionToken);

    assert!(
        !dir.join("session-token.json").exists(),
        "the sidecar is gone"
    );
    assert!(
        !dir.join("session-token.static.json").exists(),
        "the preserved mint is a long-lived credential and goes with the clear"
    );
    let p =
        crate::profile::load_profile(&crate::profile::ProfileName::from("roll")).expect("reload");
    assert!(
        !p.rolling_token,
        "the flag goes too, or the daemon re-stamps a sidecar over the clear"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.body.contains("re-stamping off")),
        "the toast names the disarm, got {:?}",
        app.toasts
    );
    // Unconditional on the removal, like the CLI's postscript: the hint's
    // pre-action disclosure is not a report, and `--yes` has no hint at all.
    assert!(
        app.toasts
            .iter()
            .any(|t| t.body.contains("the preserved mint is gone")),
        "the toast reports the destroyed backup, got {:?}",
        app.toasts
    );
}

/// The refusal gate reads the same condition as the CLI's: only a stored PIECE
/// (a sidecar or a preserved mint) is a credential the clear could strip. A
/// flag-only account with no other login therefore DISARMS — before this, the
/// widening made its row visible while the press returned silently, a surface
/// with no behavior on the one state whose only remaining fix it was.
#[test]
fn clear_session_token_disarms_a_flag_only_account_with_no_other_login() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("solo".to_string(), None, None);
    acct.rolling_token = true;
    crate::profile::save_profile(&acct).expect("save profile");

    let mut app = app_with(vec![acct]);
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("solo"),
    ));

    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    run_config_row(&mut app, ConfigRow::ClearSessionToken);

    let p =
        crate::profile::load_profile(&crate::profile::ProfileName::from("solo")).expect("reload");
    assert!(
        !p.rolling_token,
        "the disarm lands on disk, as the CLI's does"
    );
    let body = app
        .toasts
        .iter()
        .map(|t| t.body.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        body.contains("re-stamping off"),
        "the disarm is reported: {body}"
    );
    // The account holds NO credential of any kind — the api-key arms of the
    // tail were written for a different account shape, and the backup suffix
    // is gated on a removal that never happened here.
    assert!(
        body.contains("it stores no login at all"),
        "the tail must not promise an api key this account does not hold: {body}"
    );
    assert!(
        !body.contains("the preserved mint is gone"),
        "no backup existed, so none may be reported destroyed: {body}"
    );
}

/// The other-login refusal is re-checked from DISK under the guard — the TUI
/// half of the CLI's own re-check, and stricter: `reload_fingerprint` does not
/// stat `credentials.json`, so an out-of-band log-out (a script's `rm`, another
/// tool) leaves `app.config()` claiming a stored login INDEFINITELY, and the
/// in-memory gate in `run_config_row` would wave the clear through into
/// stripping the account's last credential.
#[test]
fn clear_session_token_refuses_when_the_disk_login_is_gone() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("ghost".to_string(), None, None);
    acct.credentials = Some(split_creds("stored-oauth", Some("stored-refresh")));
    crate::profile::save_profile(&acct).expect("save profile");
    seed_session_token("ghost", None);
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("ghost"))
        .expect("profile dir");

    let mut app = app_with(vec![acct]);
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("ghost"),
    ));

    // The out-of-band log-out: disk loses the login, the snapshot keeps it.
    std::fs::remove_file(dir.join("credentials.json")).expect("log out on disk");

    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    run_config_row(&mut app, ConfigRow::ClearSessionToken);

    assert!(
        dir.join("session-token.json").exists(),
        "a refused clear removes nothing"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.body.contains("no other login anymore")),
        "the refusal is loud and names the reason, got {:?}",
        app.toasts
    );
}

/// The preserved mint goes LAST, after the relink: a backup-removal failure
/// between the sidecar removal and the relink would leave an ACTIVE account's
/// live slot a dangling symlink under a "clear failed" toast — a broken login
/// reported as nothing-happened. Driven by blocking the backup slot with a
/// directory: the sidecar clears, the relink lands, and only then does the
/// removal fail, loudly and honestly.
#[test]
fn clear_session_token_relinks_before_the_backup_removal_can_fail() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("mid".to_string(), None, None);
    acct.credentials = Some(split_creds("stored-oauth", Some("stored-refresh")));
    // Armed, because only `preserve_static_mint` on the rolling path ever
    // writes the backup slot: a profile holding one and NOT armed is a shape
    // production cannot reach, and it leaves the disarm half of the report
    // unexercised.
    acct.rolling_token = true;
    crate::profile::save_profile(&acct).expect("save profile");
    seed_session_token("mid", None);
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("mid"))
        .expect("profile dir");
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("mid"))
        .expect("link");
    let live = home.home().join(".claude").join(".credentials.json");
    assert_eq!(
        std::fs::read_link(&live).expect("live is a symlink"),
        dir.join("session-token.json"),
        "fixture: the live slot starts on the sidecar"
    );
    // A directory in the backup slot fails `remove_file`, nothing else.
    std::fs::create_dir(dir.join("session-token.static.json")).expect("block the backup slot");

    let mut app = app_with(vec![acct]);
    make_active(&mut app, "mid");
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("mid"),
    ));

    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    run_config_row(&mut app, ConfigRow::ClearSessionToken);

    assert!(
        !dir.join("session-token.json").exists(),
        "the sidecar clear itself succeeded"
    );
    assert_eq!(
        std::fs::read_link(&live).expect("live survives as a symlink"),
        dir.join("credentials.json"),
        "the relink landed BEFORE the backup removal failed — no dangling live slot"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.body.contains("the preserved mint remains")),
        "the partial outcome is named, not folded into 'clear failed', got {:?}",
        app.toasts
    );
    // The relink and the disarm both already LANDED and are durable, so the
    // failure message carries them. Reporting only the mint leaves the operator
    // believing a sign-out and a stopped re-stamp are still pending.
    let body = app
        .toasts
        .iter()
        .find(|t| t.body.contains("the preserved mint remains"))
        .map(|t| t.body.clone())
        .expect("the partial-outcome toast");
    assert!(
        body.contains("relinked its own login"),
        "the failure toast must still report the relink it completed, got {body:?}"
    );
    assert!(
        body.contains("re-stamping off"),
        "the failure toast must still report the disarm it persisted, got {body:?}"
    );
}

/// The relink follows the account that is active ON DISK, never the snapshot
/// this process happens to hold. `daemon::tick` and `fallback` both reach
/// `actions::switch_profile`, so the active account moves with no keypress
/// here and `reload_fingerprint` corrects the snapshot a tick later at best.
/// Acting on the stale one skips the relink for the account that IS active and
/// leaves its live slot a dangling symlink into the file just removed, which is
/// the exact failure the relink exists to prevent.
#[cfg(unix)]
#[test]
fn clear_session_token_relinks_the_account_active_on_disk_not_in_the_snapshot() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("acct".to_string(), None, None);
    acct.credentials = Some(split_creds("stored-oauth", Some("stored-refresh")));
    crate::profile::save_profile(&acct).expect("save profile");
    seed_session_token("acct", None);
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("acct"))
        .expect("profile dir");
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("acct"))
        .expect("link");
    let live = home.home().join(".claude").join(".credentials.json");
    assert_eq!(
        std::fs::read_link(&live).expect("live is a symlink"),
        dir.join("session-token.json"),
        "fixture: the live slot starts on the sidecar"
    );

    let mut app = app_with(vec![acct]);
    make_active(&mut app, "acct");
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("acct"),
    ));
    // What an out-of-band switch away and back leaves behind: disk is current,
    // the snapshot in this process is not. Only the in-memory copy is rewound,
    // so the ONLY thing separating the two readers is which one they consult.
    app.config().state.active_profile = Some("someone-else".into());

    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    run_config_row(&mut app, ConfigRow::ClearSessionToken);

    assert!(
        !dir.join("session-token.json").exists(),
        "the clear itself must still happen"
    );
    assert_eq!(
        std::fs::read_link(&live).expect("live must survive as a symlink"),
        dir.join("credentials.json"),
        "the relink follows the on-disk active account, so the live slot never dangles"
    );
}

/// The renderer's in-memory flag flips only AFTER the persist lands. Flipped
/// first, a failed `save_profile` leaves `app.config()` claiming the flag is
/// off against a disk that still says on — `reload_fingerprint` never corrects
/// it (config mtime did not move), and any later unrelated save makes the lie
/// durable: a live rolling sidecar nothing re-stamps.
#[test]
#[cfg(unix)]
fn clear_session_token_keeps_the_flag_when_the_persist_fails() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    use std::os::unix::fs::PermissionsExt as _;
    let _home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("stuck".to_string(), None, None);
    acct.credentials = Some(split_creds("stored-oauth", Some("stored-refresh")));
    acct.rolling_token = true;
    crate::profile::save_profile(&acct).expect("save profile");
    seed_session_token("stuck", None);
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("stuck"))
        .expect("profile dir");

    let mut app = app_with(vec![acct]);
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("stuck"),
    ));

    // Fail the persist stage ALONE: a read-only profile dir fails
    // `save_profile`'s tempfile creation while every read — the under-guard
    // `load_profile`, the sidecar stats — still works, so the clear provably
    // reaches the persist and dies exactly there. The rotation lock is
    // pre-created while the dir is still writable (its parent `mkdir_700` is
    // recursive-create-only and never re-chmods an existing dir).
    drop(
        crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from("stuck"))
            .expect("pre-create the lock"),
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
        .expect("make the dir read-only");
    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("restore the dir");

    assert!(
        app.toasts.iter().any(|t| t.body.contains("clear failed")),
        "the failed persist is loud, got {:?}",
        app.toasts
    );
    assert!(
        dir.join("session-token.json").exists(),
        "a failed persist stops the clear before anything is removed"
    );
    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("stuck"))
            .is_some_and(|p| p.rolling_token),
        "the in-memory flag must not claim a disarm the disk refused"
    );
    let p =
        crate::profile::load_profile(&crate::profile::ProfileName::from("stuck")).expect("reload");
    assert!(p.rolling_token, "disk still says armed");
}

/// The TUI clear takes the rotation guard NON-BLOCKING: a rotation in flight —
/// the exact writer that could re-stamp the sidecar after the removal — fails
/// the clear loudly into a toast, and nothing on disk or in config moves. A UI
/// thread must never park behind a timeout-less flock, so `try_acquire` is the
/// only correct spelling of the CLI's load-bearing guard here.
#[test]
fn clear_session_token_refuses_while_a_rotation_holds_the_profile() {
    use super::{ConfigRow, build_draft_existing, run_config_row};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut acct = Profile::new("held".to_string(), None, None);
    acct.credentials = Some(split_creds("stored-oauth", Some("stored-refresh")));
    acct.rolling_token = true;
    crate::profile::save_profile(&acct).expect("save profile");
    seed_session_token("held", None);
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("held"))
        .expect("profile dir");

    let _outside =
        crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from("held"))
            .expect("hold the lock");

    let mut app = app_with(vec![acct]);
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_existing(
        &app,
        &crate::profile::ProfileName::from("held"),
    ));
    run_config_row(&mut app, ConfigRow::ClearSessionToken);
    run_config_row(&mut app, ConfigRow::ClearSessionToken);

    assert!(
        dir.join("session-token.json").exists(),
        "a refused clear removes nothing"
    );
    let p =
        crate::profile::load_profile(&crate::profile::ProfileName::from("held")).expect("reload");
    assert!(p.rolling_token, "a refused clear disarms nothing");
    assert!(
        app.toasts.iter().any(|t| t.body
            == crate::format::Transient::new(
                crate::format::Cause::RotationLockHeld("held".to_string()),
                crate::format::Retry::Stated,
            )
            .text()),
        "the refusal is loud, got {:?}",
        app.toasts
    );
}

/// Overview and Usage carry the focused account's own actions plus the
/// tab-global ones. The hotkeys are pinned, not scanned: `d` survives the
/// disable↔enable label flip, and `f` keeps refresh-all off `e`/`p`, which the
/// Usage page keys own.
#[test]
fn the_account_tabs_offer_the_focused_account_plus_the_global_actions() {
    use super::{Tab, build_action_menu};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let entries = |app: &super::App| -> Vec<(&'static str, Option<char>)> {
        build_action_menu(app)
            .items
            .iter()
            .map(|i| (i.label, i.hotkey))
            .collect()
    };

    let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
    app.profile_cursor = 0;

    app.tab = Tab::Overview;
    assert_eq!(
        entries(&app),
        [
            ("refresh usage", Some('r')),
            ("rotate access token", Some('t')),
            ("disable account", Some('d')),
            ("refresh all accounts", Some('f')),
            ("new account", Some('n')),
        ]
    );

    app.tab = Tab::Usage;
    assert_eq!(
        entries(&app),
        [
            ("refresh usage", Some('r')),
            ("rotate access token", Some('t')),
            ("disable account", Some('d')),
            ("refresh all accounts", Some('f')),
            ("toggle estimates", Some('e')),
            ("toggle pace marker", Some('p')),
        ]
    );

    // With no account under the cursor only the tab-global half is left, and
    // refresh-all keeps its letter rather than sliding onto the freed `r`.
    let mut empty = bare_app();
    empty.tab = Tab::Overview;
    assert_eq!(
        entries(&empty),
        [
            ("refresh all accounts", Some('f')),
            ("new account", Some('n'))
        ]
    );
}

/// The console link is offered for exactly the accounts clauth knows a page
/// for. An OAuth account has none, so the entry is absent above rather than
/// present-and-inert — the assertions there are the other direction of this one.
#[test]
fn the_provider_console_entry_follows_the_recognised_endpoints() {
    use super::{Tab, build_action_menu};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let has_entry = |app: &super::App| {
        build_action_menu(app)
            .items
            .iter()
            .any(|i| i.label == "open provider console")
    };

    let mut app = app_with(vec![
        Profile::new("oauth".to_string(), None, None),
        Profile::new(
            "qwen".to_string(),
            Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com".to_string()),
            Some("sk-sp-test".to_string()),
        ),
        Profile::new(
            "proxy".to_string(),
            Some("https://proxy.example/v1".to_string()),
            Some("sk-test".to_string()),
        ),
    ]);

    // What the entry would OPEN, not just that it is offered: the label alone
    // cannot tell a correct page from a neighbouring plan's.
    app.profile_cursor = 1;
    assert_eq!(
        super::focused_provider_console(&app),
        Some(
            "https://modelstudio.console.alibabacloud.com/ap-southeast-1?tab=plan#/efm/subscription/overview"
        )
    );

    // Every tab that carries an account scope offers it, including Setup, where
    // the api-key row it feeds lives.
    for tab in [Tab::Overview, Tab::Usage, Tab::Setup] {
        app.tab = tab;
        app.profile_cursor = 1;
        assert!(
            has_entry(&app),
            "a recognised provider offers it on {tab:?}"
        );
        app.profile_cursor = 0;
        assert!(!has_entry(&app), "an oauth account has no page on {tab:?}");
        app.profile_cursor = 2;
        assert!(
            !has_entry(&app),
            "an unrecognised endpoint has no page on {tab:?}"
        );
    }
}

/// The handler resolves the page itself instead of trusting that it was only
/// offered where one exists, so a pick that somehow lands on an account with no
/// console says so rather than opening a browser on nothing.
#[test]
fn picking_the_console_entry_without_a_page_says_so_and_opens_nothing() {
    use super::{ActionMenuAction, dispatch_action_menu_action};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("oauth".to_string(), None, None)]);
    app.profile_cursor = 0;
    dispatch_action_menu_action(&mut app, ActionMenuAction::OpenProviderConsole);

    let toast = app.toasts.back().expect("the pick answers");
    assert_eq!(toast.body, "no console page for this endpoint");
}

/// Off the Setup pane there is no `disabled` row to arm, so the menu pick routes
/// disabling through the confirm modal instead of flipping on one press —
/// enabling stays immediate, exactly as the row behaves.
#[test]
fn disabling_from_an_account_tab_confirms_first_while_enabling_is_immediate() {
    use super::{
        ActionMenuAction, ConfirmAction, Modal, Tab, build_action_menu,
        dispatch_action_menu_action, run_confirm_action,
    };
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let acct = Profile::new("acct".to_string(), None, None);
    crate::profile::save_profile(&acct).expect("save profile");
    let mut app = app_with(vec![acct]);
    app.tab = Tab::Overview;
    app.profile_cursor = 0;

    dispatch_action_menu_action(&mut app, ActionMenuAction::DisableProfile);
    assert!(
        !app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .is_disabled(),
        "the pick asks first, it does not flip"
    );
    let Some(Modal::Confirm(state)) = app.modals.pop() else {
        panic!("disabling raises a confirm");
    };
    assert!(matches!(state.on_confirm, ConfirmAction::DisableOne(ref n) if n == "acct"));

    run_confirm_action(&mut app, state.on_confirm);
    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .is_disabled()
    );

    // Re-enabling is harmless: the menu flips its label and fires on the pick.
    assert_eq!(build_action_menu(&app).items[2].label, "enable account");
    dispatch_action_menu_action(&mut app, ActionMenuAction::EnableProfile);
    assert!(
        !app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .is_disabled()
    );
    assert!(app.modals.is_empty(), "enabling asks nothing");
}

/// The active account can't be disabled. The Setup row says so by rendering
/// inert, which a menu item can't do — so the pick names the blocker instead of
/// silently doing nothing.
#[test]
fn disabling_the_active_account_from_a_menu_says_why_instead_of_no_opping() {
    use super::{ActionMenuAction, Tab, dispatch_action_menu_action};
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let acct = Profile::new("acct".to_string(), None, None);
    crate::profile::save_profile(&acct).expect("save profile");
    let mut app = super::App::new(AppConfig {
        state: AppState {
            active_profile: Some("acct".into()),
            profiles: vec!["acct".into()],
            ..AppState::default()
        },
        profiles: vec![acct],
    });
    app.tab = Tab::Overview;
    app.profile_cursor = 0;

    dispatch_action_menu_action(&mut app, ActionMenuAction::DisableProfile);
    assert!(app.modals.is_empty(), "a gated pick arms no confirm");
    assert!(
        !app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .unwrap()
            .is_disabled()
    );
    assert!(
        app.toasts.iter().any(|t| t.body.contains("switch away")),
        "the refusal names the way out"
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

/// Off macOS a live `clauth start` session no longer blocks the rotate: it reads
/// the same credential file clauth writes, so it picks the rotated pair up on
/// its next request. The action arms the ordinary rotate confirm, exactly as an
/// idle profile does — no acknowledge notice, no pre-refusal.
#[cfg(not(target_os = "macos"))]
#[test]
fn rotate_tokens_with_live_session_arms_the_rotate_confirm() {
    use super::{ActionMenuAction, ConfirmAction, Modal, dispatch_action_menu_action};
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
        .expect("a live session still arms a confirm modal");
    assert!(
        matches!(&confirm.on_confirm, ConfirmAction::RotateOne(n) if n == "busy"),
        "a live-session rotate carries RotateOne, not an acknowledge notice: {:?}",
        confirm.on_confirm
    );
}

/// The macOS arm: `rotate_one_inner` refuses there, so the action menu must say
/// so up front rather than arm a confirm whose only outcome is a silent no-op
/// behind a "rotating 'X'" toast.
#[cfg(target_os = "macos")]
#[test]
fn rotate_tokens_with_live_session_arms_an_acknowledge_notice_on_macos() {
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
        .expect("a live session arms a modal");
    assert!(
        matches!(confirm.on_confirm, ConfirmAction::Acknowledge),
        "macOS arms an acknowledge notice, not a rotate that cannot run"
    );
    assert_eq!(confirm.message, "'busy' has a live clauth start session");
    assert_eq!(
        confirm.detail.as_deref(),
        Some(super::ROTATE_LIVE_SESSION_DETAIL)
    );

    let action = confirm.on_confirm.clone();
    run_confirm_action(&mut app, action);
    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("busy"))
            .is_some(),
        "acknowledging the notice leaves the profile untouched"
    );
}

/// The macOS backstop: a session that starts between arming the confirm and
/// running it must get the refusal, never the success-shaped "rotating 'X'".
#[cfg(target_os = "macos")]
#[test]
fn confirming_a_rotate_under_a_live_session_is_refused_on_macos() {
    use super::{ConfirmAction, ToastKind, join_test_workers, run_confirm_action};
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("busy".to_string(), None, None)]);
    app.profile_cursor = 0;
    let _pid_guard = arm_live_session(home.home(), "busy");

    run_confirm_action(&mut app, ConfirmAction::RotateOne("busy".to_string()));
    join_test_workers();

    let toast = app.toasts.back().expect("the refusal is surfaced");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert_eq!(
        toast.body,
        format!(
            "'busy' {}\n{}",
            super::ROTATE_LIVE_SESSION_MSG,
            super::ROTATE_LIVE_SESSION_TOAST
        )
    );
    // Asked for, not spelled: the lock lives outside the profile directory, so
    // a hand-built profile-dir path would assert the absence of something never
    // there. What makes this absence capable of failing is that an acquire
    // MATERIALIZES the file, carried on THIS host by
    // `actions::tests::a_delete_does_not_release_the_lock_it_is_holding`, whose
    // second-thread `try_acquire` can only answer `None` against a file the
    // first acquire created and locked. The positive twin through this same TUI
    // path, `confirming_a_rotate_under_a_live_session_reaches_the_rotate`, is
    // `cfg(not(macos))` and so is compiled out exactly where this assertion
    // lives.
    assert!(
        !crate::runtime::rotation_lock_path(&crate::profile::ProfileName::from("busy"))
            .expect("rotation lock path")
            .exists(),
        "the refusal must land BEFORE the rotate worker is spawned"
    );
}

/// Arming the confirm is only half the path — `run_confirm_action` carried its
/// OWN live-session refusal, so the modal could arm and the rotate still be
/// dropped on the floor. Confirming under a live session must reach the rotate
/// and say so. The fixture profile holds no refresh token, so the spawned
/// worker short-circuits before any HTTP.
///
/// `join_test_workers` is load-bearing, not hygiene: `spawn_worker` detaches, and
/// a worker still running when `HomeSandbox` drops resolves the operator's REAL
/// `$HOME` and takes real locks under `~/.clauth`.
#[cfg(not(target_os = "macos"))]
#[test]
fn confirming_a_rotate_under_a_live_session_reaches_the_rotate() {
    use super::{ConfirmAction, ToastKind, join_test_workers, run_confirm_action};
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("busy".to_string(), None, None)]);
    app.profile_cursor = 0;
    let _pid_guard = arm_live_session(home.home(), "busy");

    run_confirm_action(&mut app, ConfirmAction::RotateOne("busy".to_string()));
    // Before any assertion, and above all before `home` drops.
    join_test_workers();

    let toast = app
        .toasts
        .back()
        .expect("confirming a rotate says something");
    assert_eq!(
        (toast.kind, toast.body.as_str()),
        (ToastKind::Info, "rotating 'busy'"),
        "a live session must not turn the rotate into a refusal"
    );
    // The worker ran to completion inside the sandbox, so its guard landed
    // there. This is the positive leg of the pair: it fires only if acquiring a
    // rotation guard still leaves this file behind, which is what makes the
    // negative assertion in the refusal test above capable of failing.
    assert!(
        crate::runtime::rotation_lock_path(&crate::profile::ProfileName::from("busy"))
            .expect("rotation lock path")
            .exists(),
        "the rotate must have taken its guard under the SANDBOX home"
    );
}

/// The rotate-all confirm's detail line is a PROMISE about what happens to a
/// running session, and the two hosts keep different ones — macOS skips such an
/// account, everywhere else the session follows the new pair. Unpinned in both
/// directions until now, so a copy edit could quietly invert it.
#[test]
fn rotate_all_detail_promises_what_the_host_actually_does() {
    let want = if cfg!(target_os = "macos") {
        "accounts with a live clauth start session are skipped."
    } else {
        "running sessions pick up the new tokens on their next request."
    };
    assert_eq!(super::ROTATE_ALL_DETAIL, want);
}

/// The single-rotate refusal copy. These strings assert a MECHANISM (clauth
/// cannot write the keychain entry that session's Claude Code reads), not the
/// old and now-false "the session manages its own tokens" theory, so a drift
/// back toward the old wording is a drift back to a wrong explanation. Pinned
/// from every platform even though only macOS renders them.
#[test]
fn the_live_session_rotate_refusal_names_the_keychain_mechanism() {
    assert_eq!(
        super::ROTATE_LIVE_SESSION_MSG,
        "has a live clauth start session"
    );
    assert_eq!(
        super::ROTATE_LIVE_SESSION_DETAIL,
        "macos keeps its login in a keychain entry clauth can't write, so rotating would sign \
         the session out."
    );
    assert_eq!(
        super::ROTATE_LIVE_SESSION_TOAST,
        "macos keeps its login where clauth can't rotate it"
    );
}

/// No live session: the rotate action arms the normal rotate confirm carrying the
/// per-profile `RotateOne`.
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
        account_uuid: uuid.map(crate::profile::AccountId::from),
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
    app.last_divergence_check = None;
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
        .send((
            1,
            Ok(super::LoginResult::Oauth(Box::new(login_outcome(
                "ref",
                Some("uuid-live"),
            )))),
        ))
        .unwrap();

    drain_login_events(&mut app);

    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("ghost"))
            .is_none(),
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
        .send((
            1,
            Ok(super::LoginResult::Oauth(Box::new(login_outcome(
                "ref",
                Some("uuid-live"),
            )))),
        ))
        .unwrap();

    drain_login_events(&mut app);

    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("fresh"))
            .is_none(),
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
        .send((
            1,
            Ok(super::LoginResult::Oauth(Box::new(login_outcome(
                "ref",
                Some("uuid-live"),
            )))),
        ))
        .unwrap();

    drain_login_events(&mut app);

    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("fresh"))
            .is_none()
    );
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
        .find(&crate::profile::ProfileName::from("fresh"))
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
            &crate::profile::ProfileName::from("fresh"),
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
            // `overwrite_captured_profile` persists `state` mid-apply, and the
            // record it writes is what the anchor seed's cache-write gate reads
            // — so the state must name the profile, as a loaded config does.
            profiles: vec!["work".into()],
            default_divergence: Some(DivergenceChoice::Overwrite),
            ..AppState::default()
        },
        profiles: vec![work],
    });
    // A reauth that swapped a DIFFERENT account onto the name: the stale anchor
    // must be replaced, or identity would keep proving the old account.
    crate::testutil::register_names(&["work"]);
    crate::usage::seed_login_anchor(
        &crate::profile::ProfileName::from("work"),
        Some(&crate::profile::AccountId::from(
            "uuid-old-account".to_string(),
        )),
    );

    apply_login(
        &mut app,
        login_session("work", false, 1),
        login_outcome("new", Some("uuid-new-account")),
    );

    assert_eq!(
        crate::profile_cache::load_profile_cache::<String>(
            &crate::profile::ProfileName::from("work"),
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
            &crate::profile::ProfileName::from("work"),
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        )
    };

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("old"));
    let mut app = App::new(AppConfig {
        state: AppState {
            // Same reason as the committed-relogin sibling: the confirmed
            // commit persists `state` mid-apply, and the record it writes is
            // what the anchor seed's cache-write gate reads.
            profiles: vec!["work".into()],
            ..AppState::default() // unset divergence default → ask first
        },
        profiles: vec![work],
    });
    // A reauth swapping a DIFFERENT account onto the name.
    crate::testutil::register_names(&["work"]);
    crate::usage::seed_login_anchor(
        &crate::profile::ProfileName::from("work"),
        Some(&crate::profile::AccountId::from(
            "uuid-old-account".to_string(),
        )),
    );

    apply_login(
        &mut app,
        login_session("work", false, 1),
        login_outcome("new", Some("uuid-new-account")),
    );

    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
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
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
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

/// `?` opens the help modal at the top and ↑↓ scrolls it, clamped both ways —
/// past the end a held ↓ would inflate the offset and make the next ↑ look
/// dead, which is exactly what the Status and Plugin detail panes clamp for.
#[test]
fn help_modal_arrows_scroll_within_the_rendered_bounds() {
    use super::{KeyCode, Modal, handle_key};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.help_scroll = 7; // a stale offset from a previous open
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('?')));
    assert!(matches!(app.modals.last(), Some(Modal::Help)));
    assert_eq!(app.help_scroll, 0, "the modal opens at the top");

    // Stands in for the render pass, which publishes the bound each frame.
    // The values are ones a real 100-col `draw_help` actually produces: 29 at a
    // 16-row terminal, 15 at 30 rows, 0 once the whole modal fits.
    app.help_max_scroll.set(29);
    for _ in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
    assert_eq!(app.help_scroll, 29, "↓ stops at the last renderable row");

    // The bound MOVES: growing the terminal shrinks the scrollable range, and
    // the offset left over from the short viewport is now past the end. ↑ must
    // land one row above the NEW bottom — stepping down from the stale 29 would
    // cost 14 presses that visibly move nothing, since the render clamps the
    // display to 15 the whole way.
    app.help_max_scroll.set(15);
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(
        app.help_scroll, 14,
        "↑ steps back from the bound, not the stale offset"
    );

    for _ in 0..30 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    }
    assert_eq!(app.help_scroll, 0, "↑ stops at the top");

    // A viewport that swallowed the whole modal collapses the range to nothing.
    app.help_max_scroll.set(0);
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(app.help_scroll, 0, "nothing to scroll, nothing moves");
    assert!(
        matches!(app.modals.last(), Some(Modal::Help)),
        "scrolling must not close the modal"
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(app.modals.is_empty(), "esc still closes it");
}

/// The same stale-offset defect the help modal had, on the two detail panes
/// that share its scroll shape. Both clamp `↓` against the bound the render
/// publishes but stepped `↑` down from whatever `detail_scroll` held, so a
/// terminal that GREW — or, here, a shorter incident selected after a long one —
/// left the offset above the new bound with the render clamping the display.
/// Every `↑` press until the offset caught up moved nothing.
#[test]
fn status_detail_up_steps_back_from_the_published_bound() {
    use super::{KeyCode, StatusFocus, Tab, handle_key};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Status;
    app.status.focus = StatusFocus::Detail;

    // Stands in for the render pass, which publishes the bound each frame.
    app.status.detail_max_scroll.set(40);
    for _ in 0..60 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
    assert_eq!(app.status.detail_scroll, 40, "↓ stops at the bound");

    app.status.detail_max_scroll.set(12);
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(
        app.status.detail_scroll, 11,
        "↑ steps back from the bound, not the stale offset"
    );
}

/// Plugin's detail pane, same defect and same fix as the Status one above.
#[test]
fn plugin_detail_up_steps_back_from_the_published_bound() {
    use super::{KeyCode, PluginFocus, Tab, handle_key};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Plugin;
    app.plugin.focus = PluginFocus::Detail;

    app.plugin.detail_max_scroll.set(40);
    for _ in 0..60 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
    assert_eq!(app.plugin.detail_scroll, 40, "↓ stops at the bound");

    app.plugin.detail_max_scroll.set(12);
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(
        app.plugin.detail_scroll, 11,
        "↑ steps back from the bound, not the stale offset"
    );
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
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
        Some("old"),
        "stored creds stay until the user confirms"
    );
    // Confirming actually lands the deferred overwrite.
    let Some(Modal::Confirm(state)) = app.modals.pop() else {
        unreachable!("asserted above");
    };
    super::run_confirm_action(&mut app, state.on_confirm);
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
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
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
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
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
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
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("work"))
        .expect("relink");
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
    let stored = crate::profile::profile_dir(&crate::profile::ProfileName::from("work"))
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
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("work"))
        .expect("link work");
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
    let work_dir =
        crate::profile::profile_dir(&crate::profile::ProfileName::from("work")).expect("work dir");
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
    let work_dir =
        crate::profile::profile_dir(&crate::profile::ProfileName::from("work")).expect("work dir");
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
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("work"))
        .expect("relink");
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
        app.config()
            .find(&crate::profile::ProfileName::from("spare"))
            .and_then(|p| p.refresh_token()),
        Some("rt-fresh"),
        "the live login landed in the chosen profile"
    );
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
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
        let config = AppConfig {
            state: AppState {
                active_profile: Some("work".into()),
                profiles: vec!["work".into(), "play".into()],
                default_divergence: Some(default),
                ..AppState::default()
            },
            profiles: vec![work, play],
        };
        crate::profile::save_app_state(&config.state).expect("persist state");
        App::new(config)
    };

    // Overwrite default + sibling-owned login: no capture, banner instead.
    let mut app = sibling_owned_app(DivergenceChoice::Overwrite);
    force_poll(&mut app);
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
        Some("rt-work"),
        "an Overwrite default must not capture play's login into work"
    );
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("play"))
            .and_then(|p| p.refresh_token()),
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
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
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
    let config = AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            profiles: vec!["work".into()],
            default_divergence: Some(DivergenceChoice::Overwrite),
            ..AppState::default()
        },
        profiles: vec![work],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    let mut app = App::new(config);
    force_poll(&mut app);
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
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
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.update_compact(13);
    assert!(app.compact);
    assert!(app.toasts.is_empty(), "compact must not fire a toast");
}

#[test]
fn compact_yields_warning_banner() {
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
fn preemptive_rotation_space_toggles_on_every_platform() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::PreemptiveRotation)
        .unwrap();
    assert!(
        app.config().state.preemptive_rotation,
        "on by default — proactive rotation is the shipped behavior"
    );

    // The lead is a clock margin, not a Keychain concern, so the row is live
    // on every platform rather than dimmed off macOS.
    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        !app.config().state.preemptive_rotation,
        "space toggles the mode off"
    );
    // Persisted to profiles.toml — the scheduler reads the flag off the shared
    // config, but a relaunch must pick it up from disk too. An explicit off is
    // the direction that regresses if the key is skipped on serialize.
    let reloaded: crate::profile::AppState = toml::from_str(
        &std::fs::read_to_string(crate::profile::clauth_dir().unwrap().join("profiles.toml"))
            .expect("read profiles.toml"),
    )
    .expect("parse profiles.toml");
    assert!(
        !reloaded.preemptive_rotation,
        "the off toggle persists to disk"
    );

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        app.config().state.preemptive_rotation,
        "space toggles the mode back on"
    );
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
    let _home = crate::testutil::HomeSandbox::new();
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
    let _home = crate::testutil::HomeSandbox::new();
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
        let _home = crate::testutil::HomeSandbox::new();
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
        let _home = crate::testutil::HomeSandbox::new();
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
        let _home = crate::testutil::HomeSandbox::new();
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
        let _home = crate::testutil::HomeSandbox::new();
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
        let _home = crate::testutil::HomeSandbox::new();
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
            ConfigRow::FableModel,
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
        let _home = crate::testutil::HomeSandbox::new();
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
        let _home = crate::testutil::HomeSandbox::new();
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
            ConfigRow::FableModel,
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
                .find(&crate::profile::ProfileName::from("acct"))
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
        Some((crate::profile::ProfileName::from("hybrid"), true, true)),
        "a stored pair is rotatable no matter where requests route"
    );

    app.profile_cursor = 1;
    assert_eq!(
        focused_account(&app),
        Some((crate::profile::ProfileName::from("apikey"), false, true)),
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
    let _home = crate::testutil::HomeSandbox::new();
    use super::update_banner;
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("a"),
    )]);
    update_banner(&mut app);
    assert_eq!(
        app.banner.as_ref().expect("banner").message,
        "no active account · select one to resume"
    );
}

#[test]
fn all_spent_banner_needs_live_spent_window() {
    let _home = crate::testutil::HomeSandbox::new();
    use super::update_banner;
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mut spent = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
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
    let _home = crate::testutil::HomeSandbox::new();
    use super::update_banner;
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mut soft = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
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
    let _home = crate::testutil::HomeSandbox::new();
    use super::update_banner;
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mut overridden = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
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
    let _home = crate::testutil::HomeSandbox::new();
    use super::update_banner;
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mut dead = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
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
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.fallback_threshold = Some(50.0);
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = 0; // FALLBACK_ROWS[0] == Threshold

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.fallback_threshold),
        Some(55.0),
        "+ still raises the threshold by 5"
    );

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.fallback_threshold),
        Some(45.0),
        "- still lowers the threshold by 5"
    );
}

// ── preferred / last_resort mutual exclusion ────────────────────────────────
//
// The two flags are contradictory ("come home here" vs "park here to the end"),
// so each toggle clears the other, and `preferred` is a radio across the chain
// exactly like `last_resort`. All three properties are asserted by driving the
// real toggle handlers, so a regression in either clear or in the radio walk
// reds here.

#[test]
fn marking_preferred_clears_last_resort_on_the_same_member() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut a = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    a.last_resort = true;
    let mut app = app_with_unlinked_profiles(vec![
        a,
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("b")),
    ]);
    app.chain_cursor = 0;

    super::toggle_preferred(&mut app);

    let cfg = app.config();
    let a = cfg
        .find(&crate::profile::ProfileName::from("a"))
        .expect("profile a");
    assert!(a.preferred, "the member is now preferred");
    assert!(
        !a.last_resort,
        "turning preferred on clears last_resort — the two never coexist"
    );
}

#[test]
fn marking_last_resort_clears_preferred_on_the_same_member() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut a = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    a.preferred = true;
    let mut app = app_with_unlinked_profiles(vec![
        a,
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("b")),
    ]);
    app.chain_cursor = 0;

    super::toggle_last_resort(&mut app);

    let cfg = app.config();
    let a = cfg
        .find(&crate::profile::ProfileName::from("a"))
        .expect("profile a");
    assert!(a.last_resort, "the member is now last resort");
    assert!(
        !a.preferred,
        "turning last_resort on clears preferred — the reciprocal of the above"
    );
}

#[test]
fn preferred_is_exclusive_across_the_chain() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut b = crate::testutil::blank_profile(&crate::profile::ProfileName::from("b"));
    b.preferred = true;
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("a")),
        b,
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("c")),
    ]);
    app.chain_cursor = 0; // select "a"

    super::toggle_preferred(&mut app);

    let cfg = app.config();
    assert!(
        cfg.find(&crate::profile::ProfileName::from("a"))
            .expect("a")
            .preferred,
        "the newly-marked member"
    );
    assert!(
        !cfg.find(&crate::profile::ProfileName::from("b"))
            .expect("b")
            .preferred,
        "the previously-preferred sibling is cleared — a radio, only one home"
    );
    assert!(
        !cfg.find(&crate::profile::ProfileName::from("c"))
            .expect("c")
            .preferred,
        "untouched member"
    );
    drop(cfg);
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == super::ToastKind::Info && t.body == "preferred moved from 'b'"),
        "a radio move names where preferred came from, so the operator sees the home account shifted",
    );
}

// A save failure on the target's own write rolls BOTH flags back — preferred
// off again and the last_resort it cleared restored — and surfaces the danger
// rather than leaving the on-disk state and the in-memory config diverged.
// Twin of `toggle_last_resort`'s rollback leg. Unix-only: it forces the failure
// by dropping write on the profiles parent, the same posture the repo's other
// save-failure probes take.
#[cfg(unix)]
#[test]
fn toggle_preferred_rolls_back_both_flags_when_the_save_fails() {
    use std::os::unix::fs::PermissionsExt;

    let home = crate::testutil::HomeSandbox::new();
    let mut a = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    a.last_resort = true;
    let mut app = app_with_unlinked_profiles(vec![
        a,
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("b")),
    ]);
    app.chain_cursor = 0;

    // Block the very first write: `save_profile` does `mkdir_700` under
    // `~/.clauth/profiles`, which fails once the home dir refuses new children.
    let restore = home.home().to_path_buf();
    std::fs::set_permissions(&restore, std::fs::Permissions::from_mode(0o500))
        .expect("chmod home read-only");

    super::toggle_preferred(&mut app);

    // Restore before any assertion so a failure still lets the sandbox clean up.
    std::fs::set_permissions(&restore, std::fs::Permissions::from_mode(0o700))
        .expect("restore home perms");

    let cfg = app.config();
    let a = cfg
        .find(&crate::profile::ProfileName::from("a"))
        .expect("profile a");
    assert!(
        !a.preferred,
        "preferred rolls back to off when its save never landed",
    );
    assert!(
        a.last_resort,
        "the last_resort the toggle cleared is restored on the rollback",
    );
    drop(cfg);
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == super::ToastKind::Danger && t.body.starts_with("save failed")),
        "the operator sees the save failure rather than a silent divergence",
    );
}

// `weekly at` joins `rotate at` as the second CONTINUOUS row (owner call,
// 2026-07-23): it now takes the same ±5 nudge alongside its existing `⏎`
// typed editor. `max spend` (a dollar ceiling) has no natural step unit and
// stays typed-only — see `fallback_max_spend_plus_minus_is_a_no_op` below.
#[test]
fn fallback_weekly_at_plus_minus_nudge_both_ways() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
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
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
        Some(75.0),
        "+ raises the weekly override by exactly 5"
    );

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
        Some(65.0),
        "- lowers the weekly override by exactly 5"
    );
}

#[test]
fn fallback_weekly_at_nudge_clamps_at_upper_bound() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.weekly_threshold = Some(99.0);
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = weekly_at_row();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
        Some(100.0),
        "+ past MAX_WEEKLY_SWITCH_PCT clamps at 100"
    );
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
        Some(100.0),
        "a further + stays pinned at the bound"
    );
}

#[test]
fn fallback_weekly_at_nudge_clamps_at_lower_bound() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    profile.weekly_threshold = Some(51.0);
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = weekly_at_row();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
        Some(50.0),
        "- past MIN_WEEKLY_SWITCH_PCT clamps at 50"
    );
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
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
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
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
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
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
    let profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a")); // weekly_threshold: None
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.config().state.weekly_switch_threshold = Some(80.0); // resolved chain default
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = weekly_at_row();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
        Some(85.0),
        "first + on an unset override sets an explicit one at resolved-default + 5"
    );
}

// `space` still opens both the weekly-at and max-spend typed editors —
// unchanged by the `+`/`-` dispatch-by-row rewrite of `handle_fallback_detail_key`.
#[test]
fn fallback_weekly_at_and_max_spend_editors_still_open_via_space() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("a"),
    )]);
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
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("a"),
    )]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = max_spend_row();
    // The ceiling is inert (editor won't open) until spend budget is armed.
    app.config().state.spend_budget_switching = true;
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.max_auto_spend),
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
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.max_auto_spend),
        Some(25.0),
        "the typed ceiling persists"
    );
}

// The ceiling is inert (dimmed) while spend budget is off — a typed value would
// do nothing, so ⏎ must not open the editor.
#[test]
fn fallback_max_spend_editor_is_inert_while_spend_budget_is_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("a"),
    )]);
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
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("a"),
    )]);
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
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.max_auto_spend),
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
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
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
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.max_auto_spend),
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
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
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
        !app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .is_some_and(|p| p.last_resort),
        "precondition: last_resort starts false"
    );

    // Simulate a stale token cache (the observable proof `refresh_tokens` ran):
    // App::new already populated it from `collect_tokens`, so clear it first.
    app.usage_tokens.lock().unwrap().clear();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .map(|p| p.last_resort),
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
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .map(|p| p.last_resort),
        Some(false),
        "⏎ toggles last_resort back off"
    );
}

// The chain has one parking spot: marking a member clears the mark everywhere
// else (radio), so two accounts can never both read `last resort ─●`.
#[test]
fn fallback_last_resort_is_exclusive_across_the_chain() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut b = crate::testutil::blank_profile(&crate::profile::ProfileName::from("b"));
    b.last_resort = true;
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("a")),
        b,
    ]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0; // member "a"
    app.fallback_detail_cursor = 4; // FALLBACK_ROWS[4] == LastResort

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .map(|p| p.last_resort),
        Some(true),
        "space marks the selected member"
    );
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("b"))
            .map(|p| p.last_resort),
        Some(false),
        "marking one member clears the previous last resort"
    );
    assert!(
        app.toasts.iter().any(|t| t.body.contains("moved from 'b'")),
        "the move away from the old member is surfaced"
    );

    // Turning the mark OFF touches nobody else.
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .map(|p| p.last_resort),
        Some(false)
    );
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("b"))
            .map(|p| p.last_resort),
        Some(false)
    );
}

// The per-account usage gates flip and persist through their toggle rows,
// independently of each other.
#[test]
fn fallback_usage_gate_toggles_persist() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("a"),
    )]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;

    app.fallback_detail_cursor = 2; // FALLBACK_ROWS[2] == CheckWeekly
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .map(|p| (p.check_weekly, p.check_scoped)),
        Some((false, true)),
        "space flips only the weekly gate"
    );

    app.fallback_detail_cursor = 3; // FALLBACK_ROWS[3] == CheckScoped
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .map(|p| (p.check_weekly, p.check_scoped)),
        Some((false, false)),
        "⏎ flips only the scoped gate"
    );

    // The off states survive a config reload from disk (persisted, not
    // just in-memory).
    let reloaded = crate::profile::load_profile(&crate::profile::ProfileName::from("a"))
        .expect("reload profile");
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
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("acme"),
    )]);

    let snapshot = crate::actions::CaptureSnapshot {
        credentials: None,
        base_url: Some("https://new.example.com".to_string()),
        api_key: Some("new-key".to_string()),
        account_uuid: None,
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
    let mut existing = crate::testutil::blank_profile(&crate::profile::ProfileName::from("acme"));
    existing.env.insert("FOO".to_string(), "bar".to_string());
    crate::profile::save_profile(&existing).expect("save existing");

    let mut app = app_with_unlinked_profiles(vec![existing]);
    app.config().state.active_profile = Some("acme".into());
    crate::profile::save_app_state(&app.config().state).expect("persist active profile");

    let config_toml = crate::profile::profile_dir(&crate::profile::ProfileName::from("acme"))
        .unwrap()
        .join("config.toml");
    let profiles_toml = crate::profile::clauth_dir().unwrap().join("profiles.toml");
    let before_config = std::fs::read(&config_toml).expect("read config.toml");
    let before_state = std::fs::read(&profiles_toml).expect("read profiles.toml");

    let snapshot = crate::actions::CaptureSnapshot {
        credentials: None,
        base_url: Some("https://new.example.com".to_string()),
        api_key: Some("new-key".to_string()),
        account_uuid: None,
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
        let _home = crate::testutil::HomeSandbox::new();
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
        let _home = crate::testutil::HomeSandbox::new();
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
                .find(&crate::profile::ProfileName::from("fresh"))
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
                .find(&crate::profile::ProfileName::from("fresh"))
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
                .find(&crate::profile::ProfileName::from("bare"))
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
    let mut p = crate::testutil::blank_profile(&crate::profile::ProfileName::from(name));
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
    let mut flagged = crate::testutil::blank_profile(&crate::profile::ProfileName::from("flagged"));
    flagged.credentials = Some(creds("flagged"));
    let mut clean = crate::testutil::blank_profile(&crate::profile::ProfileName::from("clean"));
    clean.credentials = Some(creds("clean"));

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![flagged, clean],
    };
    config.set_auth_broken(&crate::profile::ProfileName::from("flagged"), true);

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
    assert!(
        !app.config()
            .is_auth_broken(&crate::profile::ProfileName::from("dead")),
        "flag starts clear"
    );

    super::spawn_switch_gate(
        &mut app,
        crate::profile::ProfileName::from("dead".to_string()),
        |_, _| {
            Err(crate::oauth::RefreshError::Invalid(
                crate::oauth::TokenFailure::Status(400),
            ))
        },
    );
    super::drain_switch_gates(&mut app);

    assert!(
        !app.config()
            .is_active(&crate::profile::ProfileName::from("dead")),
        "a dead target must never become active"
    );
    assert!(
        app.config()
            .is_auth_broken(&crate::profile::ProfileName::from("dead")),
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
    crate::profile::save_app_state(&app.config().state).expect("persist state");

    super::spawn_switch_gate(
        &mut app,
        crate::profile::ProfileName::from("healthy".to_string()),
        |_, _| panic!("a healthy target must not spend a refresh"),
    );
    super::drain_switch_gates(&mut app);

    assert!(
        app.config()
            .is_active(&crate::profile::ProfileName::from("healthy")),
        "healthy target switches"
    );
    assert!(
        crate::usage::is_idle(&app.activity, &crate::profile::ProfileName::from("healthy")),
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

    super::spawn_switch_gate(
        &mut app,
        crate::profile::ProfileName::from("flaky".to_string()),
        |_, _| {
            Err(crate::oauth::RefreshError::Transient(
                crate::oauth::TokenFailure::Transport,
            ))
        },
    );
    super::drain_switch_gates(&mut app);

    assert!(
        !app.config()
            .is_active(&crate::profile::ProfileName::from("flaky")),
        "refused this attempt"
    );
    assert!(
        !app.config()
            .is_auth_broken(&crate::profile::ProfileName::from("flaky")),
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
    crate::profile::save_app_state(&app.config().state).expect("persist state");
    app.config()
        .set_auth_broken(&crate::profile::ProfileName::from("flagged"), true);

    super::spawn_switch_gate(
        &mut app,
        crate::profile::ProfileName::from("flagged".to_string()),
        |_, _| {
            Ok(crate::oauth::TokenResponse {
                access_token: "at-recovered".to_string(),
                refresh_token: "rt-recovered".to_string(),
                expires_in: 3600,
                scope: None,
            })
        },
    );
    super::drain_switch_gates(&mut app);

    assert!(
        app.config()
            .is_active(&crate::profile::ProfileName::from("flagged")),
        "a recovered chain switches"
    );
    assert!(
        !app.config()
            .is_auth_broken(&crate::profile::ProfileName::from("flagged")),
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
    crate::profile::save_app_state(&app.config().state).expect("persist state");

    super::spawn_switch_gate(
        &mut app,
        crate::profile::ProfileName::from("first".to_string()),
        |_, _| panic!("healthy target: no refresh"),
    );
    // Un-drained gate = switch in flight: a second switch is refused.
    super::run_confirm_action(&mut app, ConfirmAction::Switch("second".to_string()));
    assert!(
        !app.config()
            .is_active(&crate::profile::ProfileName::from("second")),
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
        !app.config()
            .is_active(&crate::profile::ProfileName::from("first")),
        "no completion under an open modal"
    );
    app.modals.pop();
    super::drain_switch_gates(&mut app);
    assert!(
        app.config()
            .is_active(&crate::profile::ProfileName::from("first")),
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
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("healthy")),
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("broken")),
    ]);
    app.config()
        .set_auth_broken(&crate::profile::ProfileName::from("broken"), true);

    super::perform_switch(&mut app, &crate::profile::ProfileName::from("broken"));
    super::drain_switch_gates(&mut app);

    assert!(
        !app.config()
            .is_active(&crate::profile::ProfileName::from("broken")),
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

/// Pins `parse_weekly_pct`'s band edges: both bounds accepted verbatim,
/// anything past them (or non-finite) rejected — the commit path and the
/// Config card's inline check both ride this one predicate.
#[test]
fn parse_weekly_pct_pins_the_band_edges() {
    use super::parse_weekly_pct;
    assert_eq!(parse_weekly_pct("50"), Some(50.0), "lower bound accepted");
    assert_eq!(parse_weekly_pct("100"), Some(100.0), "upper bound accepted");
    assert_eq!(parse_weekly_pct("97.5"), Some(97.5), "decimals accepted");
    assert_eq!(parse_weekly_pct("49.99"), None, "below the band");
    assert_eq!(parse_weekly_pct("100.1"), None, "above the band");
    assert_eq!(parse_weekly_pct("NaN"), None, "non-finite rejected");
    assert_eq!(parse_weekly_pct("inf"), None, "non-finite rejected");
    assert_eq!(parse_weekly_pct(""), None, "empty rejected");
}

// ── apply_usage Fresh-gate ─────────────────────────────────
//
// `App::apply_usage` is driven every tick over the shared usage stores. The
// bell must ring ONLY when the per-profile status is `FetchStatus::Fresh` — a
// false bell off a `RateLimited` or stale-`Cached` tick cries wolf. The three
// tests below inject the status directly into `usage_status` — the same field
// the scheduler writes on every fetch — then call `apply_usage`.
//
// The burn-rate history log is asserted here from the other side: this process
// must never WRITE it. It belongs to the fetch path (`apply_outcome`), whose
// single-fetcher lease may be held by a headless daemon, so a UI-tick writer
// would be a second one racing it. The TUI only re-reads the file on an mtime
// change, which is also covered below.
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
    let path =
        crate::profile::profile_history_path(&crate::profile::ProfileName::from(GATE_PROFILE))
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
    let mut profile =
        crate::testutil::blank_profile(&crate::profile::ProfileName::from(GATE_PROFILE));
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
    let history_path =
        crate::profile::profile_history_path(&crate::profile::ProfileName::from(GATE_PROFILE))
            .expect("profile_history_path resolves under the sandbox home");
    (app, history_path)
}

#[test]
fn apply_usage_fresh_status_fires_bell_and_never_writes_history() {
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

    // History arm: a live store entry is exactly what used to make the UI tick
    // append. The fetch path owns the log now, so even Fresh writes nothing —
    // a second writer would race whichever process holds the fetch lease.
    let after =
        std::fs::read_to_string(&history_path).expect("history file readable after apply_usage");
    assert_eq!(
        after, prior,
        "the UI tick must never write the history log (it belongs to \
         `apply_outcome`, possibly in another process)",
    );
}

/// The read half: the log is written by whichever process holds the fetch lease,
/// so a file that appeared or grew since the last look must be picked up off its
/// mtime. Written here AFTER the `App` is built, standing in for the daemon
/// landing a sample while the TUI is open.
#[test]
fn apply_usage_reloads_history_written_by_another_process() {
    let _home = crate::testutil::HomeSandbox::new();
    let (mut app, history_path) = gate_app(&_home, FetchStatus::Fresh);
    assert!(
        !app.history_cache.contains_key(GATE_PROFILE),
        "no log exists yet, so nothing is cached",
    );

    let prior = seed_prior_history_entry();
    assert!(
        history_path.exists(),
        "the external writer must have landed the log ({} bytes seeded)",
        prior.len(),
    );
    app.apply_usage();

    let cached = app
        .history_cache
        .get(GATE_PROFILE)
        .expect("the externally written log must be read into history_cache");
    assert_eq!(
        cached
            .last()
            .and_then(|(_, info)| info.five_hour.as_ref())
            .map(|w| w.utilization),
        Some(50.0),
        "and it must carry the sample that other process recorded (got {cached:?})",
    );
    let first_read = cached.len();

    // The steady-state case: the file was already read once, and the other
    // process appends to it again. A cache keyed only on the file's existence
    // would stop here and serve a stale rate for the rest of the session.
    let grown = format!(
        "{prior}{{\"ts\":{},\"name\":\"{GATE_PROFILE}\",\"usage\":{}}}\n",
        crate::usage::now_ms(),
        serde_json::to_string(&UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 65.0,
                resets_at: None,
            }),
            ..UsageInfo::default()
        })
        .expect("sample serializes"),
    );
    // The mtime watch compares `SystemTime`s, which on a coarse-granularity fs
    // can repeat within a test; push it forward explicitly rather than sleeping.
    std::fs::write(&history_path, grown).expect("append as the other process");
    crate::testutil::set_mtime(
        &history_path,
        std::time::SystemTime::now() + std::time::Duration::from_secs(2),
    );
    app.apply_usage();

    let regrown = app
        .history_cache
        .get(GATE_PROFILE)
        .expect("the log must still be cached");
    assert_eq!(
        regrown.len(),
        first_read + 1,
        "a log that GREW since the last read must be re-read, not held at the \
         first parse (got {regrown:?})",
    );
    assert_eq!(
        regrown
            .last()
            .and_then(|(_, info)| info.five_hour.as_ref())
            .map(|w| w.utilization),
        Some(65.0),
        "and the newest sample must be the one just appended",
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

    let config = AppConfig {
        state: AppState {
            active_profile: Some(BOOT_SPENT.into()),
            profiles: vec![BOOT_SPENT.into(), BOOT_SPARE.into()],
            fallback_chain: vec![BOOT_SPENT.into(), BOOT_SPARE.into()],
            ..AppState::default()
        },
        profiles: vec![spent, spare],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    let mut app = App::new(config);

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
const ALL_STATUSES: [FetchStatus; 5] = [
    FetchStatus::Fresh,
    FetchStatus::Cached,
    FetchStatus::RateLimited,
    FetchStatus::Failed,
    FetchStatus::AuthExpired,
];

/// Exhaustiveness tripwire over `FetchStatus`. The gate keys on `== Fresh`, so
/// every variant added later is non-Fresh and must be driven through the skip
/// case. An unhandled variant fails THIS match to compile, one line from the
/// [`ALL_STATUSES`] entry it also needs.
fn skips_the_one_shot(status: FetchStatus) -> bool {
    match status {
        FetchStatus::Fresh => false,
        FetchStatus::Cached
        | FetchStatus::RateLimited
        | FetchStatus::Failed
        | FetchStatus::AuthExpired => true,
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
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("a"),
    )]);
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
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
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
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
        None,
        "an empty commit clears the override"
    );

    // Out-of-range keeps the field open, writes nothing (no-toast contract).
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    for c in ['1', '5', '0'] {
        super::handle_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_key(&mut app, key(KeyCode::Enter));
    assert!(app.fallback_weekly_draft.is_some(), "invalid stays open");
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("a"))
            .and_then(|p| p.weekly_threshold),
        None
    );
}

// The override row is inert while the member's weekly gate is off — the line
// isn't judged there, so ⏎ must not open the editor.
#[test]
fn fallback_weekly_override_editor_is_inert_while_gate_is_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut a = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
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
        &crate::profile::profile_dir(&crate::profile::ProfileName::from("active"))
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

/// Two render surfaces read `App::live_sessions` every frame, so the leg that
/// refills it has to actually run on the tick — a snapshot seeded once at
/// construct would show the fleet as it was when the TUI opened, forever, and
/// every unit test of the collector would stay green saying so.
#[test]
fn a_tick_re_tallies_live_sessions_that_appeared_after_startup() {
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![Profile::new("late".to_string(), None, None)],
    });
    assert_eq!(
        app.live_sessions
            .member(&crate::profile::ProfileName::from("late"))
            .sessions,
        0,
        "the sandbox starts with an empty registry"
    );
    // Nothing must spawn a bootstrap thread out of this tick.
    app.bootstrap_started = true;

    let sid = "4242-7";
    let row = crate::live_sessions::LiveSession {
        session_id: sid.to_string(),
        start_profile: "late".to_string(),
        pid: 4242,
        started_at: 1_700_000_000_000,
        cwd: None,
        isolated: false,
        follows_chain: false,
        intended_member: None,
        chain_cursor: None,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
    };
    crate::live_sessions::register(&row).expect("register row");
    let _marker = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("late"),
        false,
        sid,
    )
    .expect("hold the session's marker");

    // From EVERY tab, not just whichever one `App::new` defaults to. The leg is
    // deliberately ungated: two surfaces read the tally, and a tab gate added
    // later to save the per-tick FS read would freeze the Fallback badge and
    // member card at their construct-time value for the process lifetime while
    // the whole suite stayed green — which is exactly what a test that never
    // touched `app.tab` could not see.
    for tab in super::Tab::ALL {
        app.tab = tab;
        app.live_sessions = crate::live_sessions::LiveTally::default();
        // Un-sampled, which the gate reads as due — past the refresh interval
        // that `App::new` starts the clock on.
        app.last_live_sessions_refresh = None;
        super::on_tick(&mut app);

        assert_eq!(
            app.live_sessions
                .member(&crate::profile::ProfileName::from("late"))
                .sessions,
            1,
            "the tally must refresh on the {tab:?} tab too"
        );
    }
}

/// Two `clauth start` children on one account plus a third on another — the only
/// shape that reaches BOTH the summary line's plural and the per-account
/// sub-line's `·` count. Every other `runtime_check_*` fixture is single-session,
/// so `instances > 1` never executed and the `{name} · {instances}` sub-line
/// shipped with no pin: reverting it left the suite fully green. The summary's
/// `account`/`accounts` split needs two hosting accounts for the same reason,
/// and only this fixture has them, so one test carries both.
#[test]
fn runtime_check_names_a_multi_session_account_with_its_count() {
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![
            Profile::new("busy".to_string(), None, None),
            Profile::new("solo".to_string(), None, None),
        ],
    });

    let mut markers = Vec::new();
    for (name, sid) in [("busy", "4242-0"), ("busy", "4242-1"), ("solo", "4343-0")] {
        crate::live_sessions::register(&crate::live_sessions::LiveSession {
            session_id: sid.to_string(),
            start_profile: name.to_string(),
            pid: 4242,
            started_at: 1_700_000_000_000,
            cwd: None,
            isolated: false,
            follows_chain: false,
            intended_member: None,
            chain_cursor: None,
            current_member: None,
            last_swap_at: None,
            launch_store: None,
        })
        .expect("register row");
        markers.push(
            crate::runtime::hold_session_row_marker(
                &crate::profile::ProfileName::from(name),
                false,
                sid,
            )
            .expect("hold the marker"),
        );
    }

    // `r`, the one path that re-collects the fleet tally these rows seed.
    super::recompute_plugin_checks(&mut app, true);

    let check = app
        .plugin
        .checks
        .iter()
        .find(|c| c.label == "runtime")
        .expect("runtime check");
    assert_eq!(
        check
            .detail
            .iter()
            .filter(|l| l.starts_with("live:") || l.starts_with("  "))
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "live: 3 across 2 accounts".to_string(),
            "  busy · 2".to_string(),
            "  solo".to_string(),
        ],
        "got {:?}",
        check.detail
    );
}

/// The singular half of the same summary line, which the fixture above cannot
/// reach: at 3 sessions across 2 accounts BOTH counts pluralize, so swapping
/// the `plural()` argument from the account count to the session count changes
/// nothing and the mutant ships green. Two sessions on ONE account is the
/// smallest shape where the two counts disagree, so it is the only shape that
/// proves the suffix tracks the accounts.
#[test]
fn runtime_check_says_one_account_when_every_live_session_shares_it() {
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![Profile::new("busy".to_string(), None, None)],
    });

    let mut markers = Vec::new();
    for sid in ["5151-0", "5151-1"] {
        crate::live_sessions::register(&crate::live_sessions::LiveSession {
            session_id: sid.to_string(),
            start_profile: "busy".to_string(),
            pid: 5151,
            started_at: 1_700_000_000_000,
            cwd: None,
            isolated: false,
            follows_chain: false,
            intended_member: None,
            chain_cursor: None,
            current_member: None,
            last_swap_at: None,
            launch_store: None,
        })
        .expect("register row");
        markers.push(
            crate::runtime::hold_session_row_marker(
                &crate::profile::ProfileName::from("busy"),
                false,
                sid,
            )
            .expect("hold the marker"),
        );
    }

    // `r`, the one path that re-collects the fleet tally these rows seed.
    super::recompute_plugin_checks(&mut app, true);

    let check = app
        .plugin
        .checks
        .iter()
        .find(|c| c.label == "runtime")
        .expect("runtime check");
    assert_eq!(
        check
            .detail
            .iter()
            .filter(|l| l.starts_with("live:") || l.starts_with("  "))
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "live: 2 across 1 account".to_string(),
            "  busy · 2".to_string()
        ],
        "got {:?}",
        check.detail
    );
}

// ── chain_would_mix ──────────────────────────────────────────────────────────
// Adding an api-key account to an all-oauth chain (or vice versa) lands a
// confirm modal. Silent on already-mixed chains, empty chains, same-kind adds,
// and unknown candidates (the add-picker never offers the last three, but the
// helper must not panic on them either).

use crate::profile::{AppConfig, AppState, Profile};
use std::collections::BTreeMap;

fn mini_profile(name: &str, api_key: Option<&str>) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: api_key.map(str::to_string),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
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
    }
}

fn cfg_with(profiles: Vec<Profile>, chain: Vec<&str>) -> AppConfig {
    let names: Vec<crate::profile::ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            profiles: names,
            fallback_chain: chain.into_iter().map(Into::into).collect(),
            ..AppState::default()
        },
        profiles,
    }
}

#[test]
fn chain_would_mix_true_when_api_key_candidate_joins_all_oauth_chain() {
    let cfg = cfg_with(
        vec![
            mini_profile("oauth_a", None),
            mini_profile("api_b", Some("sk-test")),
        ],
        vec!["oauth_a"],
    );
    assert!(super::chain_would_mix(
        &cfg,
        &crate::profile::ProfileName::from("api_b")
    ));
}

#[test]
fn chain_would_mix_true_when_oauth_candidate_joins_all_api_key_chain() {
    let cfg = cfg_with(
        vec![
            mini_profile("api_a", Some("sk-test")),
            mini_profile("oauth_b", None),
        ],
        vec!["api_a"],
    );
    assert!(super::chain_would_mix(
        &cfg,
        &crate::profile::ProfileName::from("oauth_b")
    ));
}

#[test]
fn chain_would_mix_silent_when_chain_already_mixed() {
    // Direction-agnostic: once both kinds are in the chain, another add of
    // either kind leaves the mix unchanged, so the modal is noise.
    let cfg = cfg_with(
        vec![
            mini_profile("oauth_a", None),
            mini_profile("api_b", Some("sk-test")),
            mini_profile("api_c", Some("sk-test")),
        ],
        vec!["oauth_a", "api_b"],
    );
    assert!(!super::chain_would_mix(
        &cfg,
        &crate::profile::ProfileName::from("api_c")
    ));
}

#[test]
fn chain_would_mix_silent_when_oauth_added_to_already_mixed_chain() {
    let cfg = cfg_with(
        vec![
            mini_profile("oauth_a", None),
            mini_profile("oauth_c", None),
            mini_profile("api_b", Some("sk-test")),
        ],
        vec!["oauth_a", "api_b"],
    );
    assert!(!super::chain_would_mix(
        &cfg,
        &crate::profile::ProfileName::from("oauth_c")
    ));
}

#[test]
fn chain_would_mix_silent_on_same_kind_add_to_homogeneous_chain() {
    let cfg = cfg_with(
        vec![mini_profile("oauth_a", None), mini_profile("oauth_b", None)],
        vec!["oauth_a"],
    );
    assert!(!super::chain_would_mix(
        &cfg,
        &crate::profile::ProfileName::from("oauth_b")
    ));
}

#[test]
fn chain_would_mix_silent_on_empty_chain() {
    let cfg = cfg_with(
        vec![
            mini_profile("oauth_a", None),
            mini_profile("api_b", Some("sk-test")),
        ],
        vec![],
    );
    assert!(!super::chain_would_mix(
        &cfg,
        &crate::profile::ProfileName::from("api_b")
    ));
    assert!(!super::chain_would_mix(
        &cfg,
        &crate::profile::ProfileName::from("oauth_a")
    ));
}

#[test]
fn chain_would_mix_returns_false_for_unknown_candidate() {
    let cfg = cfg_with(vec![mini_profile("oauth_a", None)], vec!["oauth_a"]);
    assert!(!super::chain_would_mix(
        &cfg,
        &crate::profile::ProfileName::from("ghost")
    ));
}

// ── Enter-arm wiring ────────────────────────────────────────────────────────
// Drives the production `handle_fallback_add_key` Enter path end-to-end. Two
// gaps closed at once: (1) removing the `if would_mix` gate (always calling
// `commit_chain_add`) reds here — the modal vanishes; (2) editing the
// message/detail literals in `handle_fallback_add_key` reds here — the pin
// reads them back off the raised `Modal::Confirm`. The render-pin test in
// `tui_render_modals.rs` covers `draw_confirm`'s shape; this one covers the
// handler's contract.
#[test]
fn fallback_add_enter_raises_confirm_modal_when_add_would_mix_kinds() {
    use crate::profile::{AppConfig, AppState};

    let _home = crate::testutil::HomeSandbox::new();

    let oauth_member = crate::testutil::blank_profile(&crate::profile::ProfileName::from("alice"));
    let mut api_candidate =
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("bob"));
    api_candidate.api_key = Some("sk-test".to_string());

    let profiles = vec![oauth_member, api_candidate];
    let names: Vec<crate::profile::ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: names.clone(),
            // Homogeneous oauth chain — adding `bob` (api-key) would create the mix.
            fallback_chain: vec!["alice".into()],
            ..AppState::default()
        },
        profiles,
    });
    app.tab = super::Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    // `chain_cursor` sits on the `+ add` row (members precede it); the add
    // picker reads candidates from `chain_candidates`, so cursor 0 == `bob`.
    app.chain_cursor = 1;
    app.fallback_detail_cursor = 0;

    super::handle_fallback_add_key(&mut app, crate::testutil::key(super::KeyCode::Enter));

    let confirm = app
        .modals
        .last()
        .and_then(|m| match m {
            super::Modal::Confirm(s) => Some(s),
            _ => None,
        })
        .expect("mix-creating add raises a confirm modal");
    assert!(
        matches!(&confirm.on_confirm, super::ConfirmAction::AddChainCandidate(n) if n == "bob"),
        "confirm carries AddChainCandidate(\"bob\"), got {:?}",
        confirm.on_confirm
    );
    assert_eq!(
        confirm.message,
        "mixing api-key and oauth accounts can leave sessions stuck on the api account.",
        "production message copy in handle_fallback_add_key drifted"
    );
    assert_eq!(
        confirm.detail.as_deref(),
        Some("api → oauth switches may not work until cc restarts."),
        "production detail copy in handle_fallback_add_key drifted"
    );
    // And the member has NOT been added — confirm must carry through, not preempt.
    assert!(
        !app.config().state.fallback_chain.iter().any(|n| n == "bob"),
        "the candidate must not enter the chain until the confirm runs"
    );
}

#[test]
fn fallback_add_enter_commits_directly_when_add_would_not_mix() {
    use crate::profile::{AppConfig, AppState};

    let _home = crate::testutil::HomeSandbox::new();

    // `blank_profile` defaults to `api_key: None`, so both are oauth — the add
    // is same-kind to a homogeneous chain and must skip the modal.
    let profiles = vec![
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("alice")),
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("carol")),
    ];
    let names: Vec<crate::profile::ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: names.clone(),
            fallback_chain: vec!["alice".into()],
            ..AppState::default()
        },
        profiles,
    });
    app.tab = super::Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 1;
    app.fallback_detail_cursor = 0;

    super::handle_fallback_add_key(&mut app, crate::testutil::key(super::KeyCode::Enter));

    assert!(
        app.modals.is_empty(),
        "same-kind add must not raise a modal, got {:?}",
        app.modals
    );
    assert!(
        app.config()
            .state
            .fallback_chain
            .iter()
            .any(|n| n == "carol"),
        "same-kind add commits directly without a confirm"
    );
}

// ── `● daemon` header dot at startup ─────────────────────────────────────────

/// The dot's health must be PROBED at construct, never seeded with a constant.
/// The first paint happens before any `on_tick` and `poll_daemon_health` is
/// throttled to 1 Hz, so a seeded `Absent` renders "no daemon runs" as fact for
/// the first second of every launch while one is live. Two legs on one sandbox:
/// with the singleton flock held the seed reads non-`Absent` (the dot shows),
/// with it released `Absent` (the dot hides) — so no constant seed passes both.
#[test]
fn construct_probes_the_daemon_dot_instead_of_seeding_a_constant() {
    use crate::daemon::{DaemonHealth, hold_daemon_lock};

    let _home = crate::testutil::HomeSandbox::new();

    let held = hold_daemon_lock();
    assert_eq!(
        app_with(Vec::new()).daemon_health,
        DaemonHealth::Stale,
        "a live daemon that has not published status.json yet → amber on frame 1"
    );

    drop(held);
    assert_eq!(
        app_with(Vec::new()).daemon_health,
        DaemonHealth::Absent,
        "no holder → the seed hides the dot, proving the probe reads the lock"
    );
}

// ── Setup menu: duplicate + presets ───────────────────────────────────────────
//
// The Setup pane's menu is three whole-account actions, none of which any key
// reaches: every per-row action is already the row's own ⏎.

/// Both halves of the Setup tab configure the same focused account, so both
/// carry the same scoped trio under the account's name. Past the roster (`+
/// new`) only `apply preset` is offered — there is no source account to
/// duplicate or save, but stamping a template onto the draft is the primary
/// reason a preset exists.
#[test]
fn the_setup_tab_offers_the_focused_accounts_whole_account_actions() {
    use super::{ConfigFocus, Tab, build_action_menu};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
    app.tab = Tab::Setup;
    app.profile_cursor = 0;

    for focus in [ConfigFocus::Profiles, ConfigFocus::Actions] {
        app.config_focus = focus;
        let menu = build_action_menu(&app);
        assert_eq!(
            menu.items
                .iter()
                .map(|i| (i.label, i.hotkey))
                .collect::<Vec<_>>(),
            [
                ("duplicate account", Some('d')),
                ("save as preset", Some('s')),
                ("apply preset", Some('p')),
            ],
            "{focus:?} carries the account-scoped trio",
        );
        assert_eq!(menu.scoped_len, 3, "all three act on the account");
        assert_eq!(menu.context.as_deref(), Some("acct"));
    }

    // `+ new` sits past the roster: only `apply preset` is offered, scoped to
    // the draft (no context name until the user types one).
    app.profile_cursor = app.profile_count();
    let menu = build_action_menu(&app);
    assert_eq!(
        menu.items
            .iter()
            .map(|i| (i.label, i.hotkey))
            .collect::<Vec<_>>(),
        [("apply preset", Some('p'))],
        "`+ new` offers apply preset only",
    );
    assert_eq!(menu.scoped_len, 1);
    assert_eq!(menu.context, None, "the draft has no name yet");
}

/// Applying a preset on `+ new` stamps the draft's input buffers (base_url +
/// model), not a saved profile — nothing hits disk until the create form fires.
#[test]
fn apply_preset_on_new_row_stamps_the_draft_buffers() {
    use super::{ActionMenuAction, Tab, dispatch_action_menu_action, handle_key};
    use crate::profile::Profile;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
    app.tab = Tab::Setup;
    app.profile_cursor = app.profile_count();
    app.config_draft = Some(super::build_draft_new());

    // Open the picker (cursor 0 = DeepSeek built-in), press Enter to pick it.
    // No confirm fires: the draft has no saved fields to clobber.
    dispatch_action_menu_action(&mut app, ActionMenuAction::ApplyPreset);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));

    let draft = app.config_draft.as_ref().expect("draft still mounted");
    assert_eq!(draft.base_url.value, "https://api.deepseek.com/anthropic");
    assert_eq!(draft.model.value, "deepseek-chat");
    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("acct"))
            .is_some_and(|p| p.base_url.is_none()),
        "the saved profile is untouched"
    );
}

/// `duplicate account` copies every configured field. The stored login does NOT
/// come along, and neither do the two chain radios — `preferred` is exclusive
/// across the whole roster (`toggle_preferred` clears every sibling), so a copy
/// would put two profiles in a slot only one may hold.
#[test]
fn duplicate_copies_the_settings_and_leaves_the_login_and_the_radios_behind() {
    use super::{ActionMenuAction, Modal, Tab, dispatch_action_menu_action, handle_key};
    use crate::profile::{ClaudeCredentials, OAuthToken, Profile};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut src = Profile::new(
        "src".to_string(),
        Some("https://api.test/anthropic".to_string()),
        Some("sk-secret".to_string()),
    );
    src.env.insert("FOO".to_string(), "bar".to_string());
    src.models.default = Some("deepseek-chat".to_string());
    src.models.fable = Some("claude-fable-5".to_string());
    src.auto_start = true;
    src.fallback_threshold = Some(80.0);
    src.bell_threshold = Some(95.0);
    src.check_weekly = false;
    src.preferred = true;
    src.last_resort = true;
    src.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at".to_string(),
            refresh_token: Some("rt".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&src).expect("save source");

    let mut app = app_with(vec![src]);
    app.tab = Tab::Setup;
    app.profile_cursor = 0;

    dispatch_action_menu_action(&mut app, ActionMenuAction::Duplicate);
    assert!(
        matches!(app.modals.last(), Some(Modal::NamePrompt(_))),
        "the copy waits on a name",
    );
    for ch in "copy".chars() {
        handle_key(&mut app, crate::testutil::key(KeyCode::Char(ch)));
    }
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(app.modals.is_empty(), "the prompt closes on commit");

    let cfg = app.config();
    let copy = cfg
        .find(&crate::profile::ProfileName::from("copy"))
        .expect("the duplicate exists");
    assert_eq!(copy.base_url.as_deref(), Some("https://api.test/anthropic"));
    assert_eq!(copy.api_key.as_deref(), Some("sk-secret"));
    assert_eq!(copy.env.get("FOO").map(String::as_str), Some("bar"));
    assert_eq!(
        copy.models,
        cfg.find(&crate::profile::ProfileName::from("src"))
            .expect("source")
            .models
    );
    assert!(copy.auto_start);
    assert_eq!(copy.fallback_threshold, Some(80.0));
    assert_eq!(copy.bell_threshold, Some(95.0));
    assert!(!copy.check_weekly, "an off-by-default gate copies as off");

    assert!(copy.credentials.is_none(), "the stored login stays behind");
    assert!(!copy.preferred, "preferred is a roster-wide radio");
    assert!(!copy.last_resort, "so is last-resort");
    assert!(
        cfg.find(&crate::profile::ProfileName::from("src"))
            .expect("source")
            .preferred,
        "the source keeps its own radio",
    );
}

/// A duplicate named after an existing account is refused by the same validator
/// the create form uses, with the prompt left open so the name can be fixed.
#[test]
fn duplicate_refuses_a_name_already_on_the_roster() {
    use super::{ActionMenuAction, Modal, Tab, dispatch_action_menu_action, handle_key};
    use crate::profile::Profile;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let src = Profile::new("src".to_string(), None, None);
    crate::profile::save_profile(&src).expect("save source");
    // The validator reads the roster off DISK (cross-harness uniqueness), so
    // the taken name must be in the stored state, not only in the in-memory
    // fixture.
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec!["src".into(), "taken".into()],
        ..Default::default()
    })
    .expect("save roster");
    let mut app = app_with(vec![src, Profile::new("taken".to_string(), None, None)]);
    app.tab = Tab::Setup;
    app.profile_cursor = 0;

    dispatch_action_menu_action(&mut app, ActionMenuAction::Duplicate);
    for ch in "taken".chars() {
        handle_key(&mut app, crate::testutil::key(KeyCode::Char(ch)));
    }
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));

    assert!(
        matches!(app.modals.last(), Some(Modal::NamePrompt(_))),
        "the prompt stays open on a bad name",
    );
    assert_eq!(
        app.config().profiles.len(),
        2,
        "nothing was created under the taken name",
    );
}

/// `save as preset` stores the account's endpoint + models, and `apply preset`
/// stamps them onto another account. The picked preset is re-read from disk at
/// apply, so what lands is what the store holds.
#[test]
fn a_saved_preset_applies_onto_another_account() {
    use super::{ActionMenuAction, Modal, Tab, dispatch_action_menu_action, handle_key};
    use crate::profile::Profile;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut src = Profile::new(
        "src".to_string(),
        Some("https://api.test/anthropic".to_string()),
        None,
    );
    src.models.default = Some("deepseek-chat".to_string());
    src.models.fable = Some("claude-fable-5".to_string());
    crate::profile::save_profile(&src).expect("save source");
    // The target holds a key of its own: a template names an endpoint, never
    // the credential for one, so the apply must leave it standing.
    let mut target = Profile::new("target".to_string(), None, Some("sk-target".to_string()));
    target.api_key = Some("sk-target".to_string());
    crate::profile::save_profile(&target).expect("save target");

    let mut app = app_with(vec![src, target]);
    app.tab = Tab::Setup;
    app.profile_cursor = 0;

    dispatch_action_menu_action(&mut app, ActionMenuAction::SaveAsPreset);
    for ch in "mine".chars() {
        handle_key(&mut app, crate::testutil::key(KeyCode::Char(ch)));
    }
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let saved = crate::presets::load_preset("mine").expect("the preset landed");
    assert_eq!(
        saved.base_url.as_deref(),
        Some("https://api.test/anthropic")
    );
    assert_eq!(saved.models.fable.as_deref(), Some("claude-fable-5"));

    // Apply it onto the blank second account: nothing is set there, so no
    // warning stands between the pick and the write.
    app.profile_cursor = 1;
    dispatch_action_menu_action(&mut app, ActionMenuAction::ApplyPreset);
    let Some(Modal::PresetPicker(picker)) = app.modals.last() else {
        panic!("apply opens the picker");
    };
    let at = picker
        .presets
        .iter()
        .position(|p| p.name == "mine")
        .expect("the saved preset is listed");
    let Some(Modal::PresetPicker(picker)) = app.modals.last_mut() else {
        unreachable!()
    };
    picker.cursor = at;
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));

    assert!(
        app.modals.is_empty(),
        "a blank target applies straight away"
    );
    let cfg = app.config();
    let applied = cfg
        .find(&crate::profile::ProfileName::from("target"))
        .expect("target");
    assert_eq!(
        applied.base_url.as_deref(),
        Some("https://api.test/anthropic")
    );
    assert_eq!(applied.models.default.as_deref(), Some("deepseek-chat"));
    assert_eq!(applied.models.fable.as_deref(), Some("claude-fable-5"));
    assert_eq!(
        applied.api_key.as_deref(),
        Some("sk-target"),
        "the account's own key survives an endpoint swap",
    );
}

/// Applying over an account that already carries an endpoint or model settings
/// names the fields it would replace, and cancelling leaves them alone.
#[test]
fn applying_over_set_fields_names_them_before_replacing_anything() {
    use super::{
        ActionMenuAction, Modal, Tab, dispatch_action_menu_action, handle_key, run_confirm_action,
    };
    use crate::profile::Profile;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut target = Profile::new(
        "target".to_string(),
        Some("https://old.test".to_string()),
        None,
    );
    target.models.default = Some("keep-me".to_string());
    target.models.opus = Some("old-opus".to_string());
    crate::profile::save_profile(&target).expect("save target");

    let mut app = app_with(vec![target]);
    app.tab = Tab::Setup;
    app.profile_cursor = 0;

    dispatch_action_menu_action(&mut app, ActionMenuAction::ApplyPreset);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter)); // cursor 0 = DeepSeek

    let Some(Modal::Confirm(state)) = app.modals.pop() else {
        panic!("a set field raises the overwrite warning");
    };
    assert_eq!(state.message, "apply 'DeepSeek' over 'target'?");
    assert_eq!(
        state.detail.as_deref(),
        Some("replaces base url, model, opus."),
        "the warning names the fields, not just that there are some",
    );
    assert!(!state.choice, "the warning defaults to cancel");

    // Cancelling is the pop above — nothing ran.
    assert_eq!(
        app.config()
            .find(&crate::profile::ProfileName::from("target"))
            .expect("target")
            .base_url
            .as_deref(),
        Some("https://old.test"),
        "the cancelled apply wrote nothing",
    );

    run_confirm_action(&mut app, state.on_confirm);
    let cfg = app.config();
    let applied = cfg
        .find(&crate::profile::ProfileName::from("target"))
        .expect("target");
    assert_eq!(
        applied.base_url.as_deref(),
        Some("https://api.deepseek.com/anthropic")
    );
    assert_eq!(applied.models.default.as_deref(), Some("deepseek-chat"));
    assert_eq!(
        applied.models.opus, None,
        "the apply replaces the model block whole, it does not merge into it",
    );
}

/// `save as preset` onto a name a custom preset already holds asks first, and
/// a built-in's name is refused outright with the prompt still open.
#[test]
fn saving_a_preset_guards_both_an_existing_name_and_a_builtin() {
    use super::{
        ActionMenuAction, ConfirmAction, Modal, Tab, dispatch_action_menu_action, handle_key,
    };
    use crate::profile::Profile;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let src = Profile::new(
        "src".to_string(),
        Some("https://api.test".to_string()),
        None,
    );
    crate::profile::save_profile(&src).expect("save source");
    let mut app = app_with(vec![src]);
    app.tab = Tab::Setup;
    app.profile_cursor = 0;

    let type_name = |app: &mut super::App, name: &str| {
        dispatch_action_menu_action(app, ActionMenuAction::SaveAsPreset);
        for ch in name.chars() {
            handle_key(app, crate::testutil::key(KeyCode::Char(ch)));
        }
        handle_key(app, crate::testutil::key(KeyCode::Enter));
    };

    type_name(&mut app, "mine");
    assert!(
        crate::presets::preset_exists("mine"),
        "the first save lands"
    );

    type_name(&mut app, "mine");
    let Some(Modal::Confirm(state)) = app.modals.pop() else {
        panic!("a second save over the same name asks first");
    };
    assert!(matches!(
        state.on_confirm,
        ConfirmAction::OverwritePreset(ref p, ref s) if p == "mine" && s == "src"
    ));

    type_name(&mut app, "DeepSeek");
    assert!(
        matches!(app.modals.last(), Some(Modal::NamePrompt(_))),
        "a built-in name is refused with the prompt open so it can be retyped",
    );
    assert!(
        !crate::presets::preset_exists("DeepSeek"),
        "and nothing was written into the built-in's slot",
    );
}

/// Pressing `d` on a built-in preset toasts "always available" with the picker
/// still mounted — the user can pick another or back out. Pressing `d` on a
/// custom preset pops the picker and raises the delete confirm.
#[test]
fn d_on_a_builtin_keeps_the_picker_on_a_custom_pops_it() {
    use super::{
        ActionMenuAction, ConfirmAction, Modal, Tab, dispatch_action_menu_action, handle_key,
    };
    use crate::profile::{ModelSettings, Profile};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    // Seed a custom preset so there's something to delete.
    crate::presets::save_preset(
        "mine",
        &Some("https://custom.test".to_string()),
        &ModelSettings::default(),
    )
    .expect("save custom preset");

    let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
    app.tab = Tab::Setup;
    app.profile_cursor = 0;

    // Cursor 0 = DeepSeek built-in. `d` toasts and keeps the picker mounted.
    dispatch_action_menu_action(&mut app, ActionMenuAction::ApplyPreset);
    handle_key(&mut app, key(KeyCode::Char('d')));
    assert!(
        matches!(app.modals.last(), Some(Modal::PresetPicker(_))),
        "the picker stays mounted on a built-in `d`"
    );
    // Back out, then walk down to the custom preset. Its index is read off the
    // same list the picker renders rather than hard-coded, so growing the
    // built-in table moves the cursor instead of silently retargeting this
    // assertion at a built-in.
    handle_key(&mut app, key(KeyCode::Esc));
    dispatch_action_menu_action(&mut app, ActionMenuAction::ApplyPreset);
    let mine = crate::presets::list_presets()
        .iter()
        .position(|p| p.name == "mine")
        .expect("the saved preset is in the picker's list");
    for _ in 0..mine {
        handle_key(&mut app, key(KeyCode::Down));
    }
    handle_key(&mut app, key(KeyCode::Char('d')));
    assert!(
        matches!(app.modals.last(), Some(Modal::Confirm(_))),
        "the picker pops and the confirm takes over on a custom `d`"
    );
    if let Some(Modal::Confirm(state)) = app.modals.last() {
        assert!(matches!(
            state.on_confirm,
            ConfirmAction::DeletePreset(ref n) if n == "mine"
        ));
    }
}

// ── Alibaba console login from the Setup `log in` row ─────────────────────────

fn console_outcome(token: &str) -> crate::alibaba_login::ConsoleLoginOutcome {
    crate::alibaba_login::ConsoleLoginOutcome {
        console: crate::profile::ConsoleCredential {
            token: token.to_string(),
            site: crate::profile::ConsoleSite::International,
            region: "ap-southeast-1".to_string(),
        },
    }
}

/// `log in` means a different flow per account, and the row cannot show which.
/// An Alibaba account's usage rides a console session its api key cannot stand
/// in for, so that row captures the session — matching a bare `clauth login`.
/// Every other account keeps the flow it had.
#[test]
fn the_login_row_targets_a_console_only_for_a_model_studio_account() {
    use crate::profile::{ConsoleSite, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let app = app_with(vec![
        Profile::new("oauth".to_string(), None, None),
        Profile::new(
            "qwen-intl".to_string(),
            Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string()),
            Some("sk-sp-test".to_string()),
        ),
        Profile::new(
            "qwen-cn".to_string(),
            Some("https://coding.dashscope.aliyuncs.com/apps/anthropic".to_string()),
            Some("sk-sp-test".to_string()),
        ),
        Profile::new(
            "deepseek".to_string(),
            Some("https://api.deepseek.com".to_string()),
            Some("sk-test".to_string()),
        ),
        Profile::new(
            "proxy".to_string(),
            Some("https://proxy.example/v1".to_string()),
            Some("sk-test".to_string()),
        ),
    ]);

    // Exact values: the site decides which console front is opened, and a token
    // minted on one front is meaningless on the other.
    assert_eq!(
        super::console_login_target(&app, &crate::profile::ProfileName::from("qwen-intl")),
        Some((ConsoleSite::International, "ap-southeast-1"))
    );
    assert_eq!(
        super::console_login_target(&app, &crate::profile::ProfileName::from("qwen-cn")),
        Some((ConsoleSite::Domestic, "cn-beijing"))
    );

    for other in ["oauth", "deepseek", "proxy", "missing"] {
        assert_eq!(
            super::console_login_target(&app, &crate::profile::ProfileName::from(other)),
            None,
            "'{other}' keeps its own login flow"
        );
    }
}

/// The captured session lands on the profile and nothing else moves: the api
/// key stays, because the console hands back a workspace key billed against a
/// different product than the plan this account runs on.
#[test]
fn a_captured_console_session_replaces_only_the_session() {
    use super::drain_login_events;
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let acct = Profile::new(
        "qwen".to_string(),
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string()),
        Some("sk-sp-original".to_string()),
    );
    crate::profile::save_profile(&acct).expect("save profile");
    let mut app = app_with(vec![acct]);

    app.login_generation = 1;
    app.login = Some(login_session("qwen", false, 1));
    app.login_result_tx
        .send((
            1,
            Ok(super::LoginResult::Console(Box::new(console_outcome(
                "console-token-1",
            )))),
        ))
        .unwrap();

    drain_login_events(&mut app);

    let cfg = app.config();
    let profile = cfg
        .find(&crate::profile::ProfileName::from("qwen"))
        .expect("profile survives the login");
    let console = profile.console.as_ref().expect("session stored");
    assert_eq!(console.token, "console-token-1");
    assert_eq!(console.region, "ap-southeast-1");
    assert_eq!(
        profile.api_key.as_deref(),
        Some("sk-sp-original"),
        "the console's own workspace key must never reach the profile"
    );
    assert_eq!(
        profile.base_url.as_deref(),
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic"),
        "the endpoint is untouched"
    );
    drop(cfg);
    // Storing it is half the job: `store_console_login` drops the cache the old
    // session filled, so without the re-fetch the tab shows nothing until the
    // next cadence tick.
    assert!(
        app.refetch_queue.lock().unwrap().contains("qwen"),
        "a fresh session asks for the figures it can now read"
    );
}

/// A browser round-trip is long enough for the account to be repointed at
/// another endpoint underneath it. A session is only meaningful on the console
/// its endpoint is administered from, so the apply re-checks instead of storing
/// it against whatever the profile has become.
#[test]
fn a_console_session_is_discarded_when_the_account_stopped_being_alibaba() {
    use super::drain_login_events;
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let acct = Profile::new(
        "moved".to_string(),
        Some("https://api.deepseek.com".to_string()),
        Some("sk-test".to_string()),
    );
    crate::profile::save_profile(&acct).expect("save profile");
    let mut app = app_with(vec![acct]);

    app.login_generation = 1;
    app.login = Some(login_session("moved", false, 1));
    app.login_result_tx
        .send((
            1,
            Ok(super::LoginResult::Console(Box::new(console_outcome(
                "console-token-2",
            )))),
        ))
        .unwrap();

    drain_login_events(&mut app);

    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("moved"))
            .unwrap()
            .console
            .is_none(),
        "the session must not be stored against a non-Alibaba endpoint"
    );
    let toast = app.toasts.back().expect("the discard is reported");
    assert!(
        toast.body.contains("no longer points at the console"),
        "the toast names the reason, got {:?}",
        toast.body
    );
}

/// The dangerous half of the same race, and the reason the re-check is on the
/// SITE rather than on "still Alibaba": both Model Studio fronts pass a
/// provider check, and the usage fetch keys on the credential's own site rather
/// than on `base_url`. Storing an international session against a mainland
/// endpoint would not read as a dead session, it would report the other plan's
/// quota under this account's name.
#[test]
fn a_console_session_from_the_other_front_is_discarded_rather_than_stored() {
    use super::drain_login_events;
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let acct = Profile::new(
        "swapped".to_string(),
        // Mainland endpoint; the captured session below is international.
        Some("https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic".to_string()),
        Some("sk-sp-test".to_string()),
    );
    crate::profile::save_profile(&acct).expect("save profile");
    let mut app = app_with(vec![acct]);

    assert_eq!(
        super::console_login_target(&app, &crate::profile::ProfileName::from("swapped"))
            .map(|(site, _)| site),
        Some(crate::profile::ConsoleSite::Domestic),
        "the fixture is the mainland front, so the intl session below mismatches"
    );

    app.login_generation = 1;
    app.login = Some(login_session("swapped", false, 1));
    app.login_result_tx
        .send((
            1,
            Ok(super::LoginResult::Console(Box::new(console_outcome(
                "intl-token",
            )))),
        ))
        .unwrap();

    drain_login_events(&mut app);

    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("swapped"))
            .unwrap()
            .console
            .is_none(),
        "a session from the other console front must not be stored"
    );
}

/// A profile deleted during the browser round-trip is a different failure from
/// one that moved, and the operator is told which. They shared one message
/// until 2026-08-11, so a delete reported the account had changed type.
#[test]
fn a_console_session_for_a_deleted_account_says_it_is_gone() {
    use super::drain_login_events;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![]);
    app.login_generation = 1;
    app.login = Some(login_session("vanished", false, 1));
    app.login_result_tx
        .send((
            1,
            Ok(super::LoginResult::Console(Box::new(console_outcome(
                "orphan-token",
            )))),
        ))
        .unwrap();

    drain_login_events(&mut app);

    let toast = app.toasts.back().expect("the discard is reported");
    assert!(
        toast.body.contains("no longer exists"),
        "a deleted account is reported as gone, got {:?}",
        toast.body
    );
}

/// The branch ORDER is the whole change: an Alibaba account satisfies the
/// api-key predicate too, so putting the console branch second routes every one
/// of them back to the api-key re-entry with nothing else failing. Testing the
/// two predicates in isolation cannot see that, so pin the overlap.
#[test]
fn a_model_studio_account_satisfies_both_login_predicates() {
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let app = app_with(vec![Profile::new(
        "qwen".to_string(),
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string()),
        Some("sk-sp-test".to_string()),
    )]);

    assert!(
        !app.config()
            .find(&crate::profile::ProfileName::from("qwen"))
            .unwrap()
            .login_is_oauth(),
        "the api-key arm's predicate is TRUE for it, which is why order decides"
    );
    assert_eq!(
        super::login_row_flow(&app, Some("qwen")),
        super::LoginRowFlow::Console {
            site: crate::profile::ConsoleSite::International,
            region: "ap-southeast-1",
        },
        "and the console arm is the one that wins"
    );
}

/// The other two arms of the same resolver, so the console arm cannot be
/// widened into them unnoticed.
#[test]
fn the_login_row_keeps_its_other_two_flows() {
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let app = app_with(vec![
        Profile::new("oauth".to_string(), None, None),
        Profile::new(
            "deepseek".to_string(),
            Some("https://api.deepseek.com".to_string()),
            Some("sk-test".to_string()),
        ),
    ]);

    assert_eq!(
        super::login_row_flow(&app, Some("deepseek")),
        super::LoginRowFlow::ApiKey
    );
    assert_eq!(
        super::login_row_flow(&app, Some("oauth")),
        super::LoginRowFlow::OauthMint
    );
    assert_eq!(
        super::login_row_flow(&app, Some("missing")),
        super::LoginRowFlow::OauthMint,
        "an unknown name cannot re-enter a key it has no account for"
    );
    assert_eq!(
        super::login_row_flow(&app, None),
        super::LoginRowFlow::OauthMint,
        "the `+ new` form has no account to read a type off"
    );
}

// ── herdr row (Plugin tab) ──────────────────────────────────────────────────

use crate::herdr::{ConfigStatus, HerdrProbe, RegistryEntry, SidebarState};

fn herdr_entry(enabled: bool, min: Option<&str>, warnings: Vec<&str>) -> RegistryEntry {
    RegistryEntry {
        enabled,
        version: Some("0.1.0".into()),
        min_herdr_version: min.map(str::to_string),
        plugin_root: None,
        source_kind: Some("github".into()),
        warnings: warnings.into_iter().map(str::to_string).collect(),
    }
}

fn herdr_probe(
    version: Option<&str>,
    entry: Option<RegistryEntry>,
    error: Option<&str>,
) -> HerdrProbe {
    HerdrProbe {
        version: version.map(str::to_string),
        entry,
        config_path: Some(std::path::PathBuf::from("/tmp/herdr/config.toml")),
        error: error.map(str::to_string),
    }
}

fn herdr_config(parsed: bool, key: Option<&str>, sidebar: SidebarState) -> ConfigStatus {
    ConfigStatus {
        parsed,
        bound_key: key.map(str::to_string),
        sidebar,
    }
}

fn healthy_herdr_probe() -> HerdrProbe {
    herdr_probe(
        Some("0.8.0"),
        Some(herdr_entry(true, Some("0.8.0"), vec![])),
        None,
    )
}

fn healthy_herdr_config() -> ConfigStatus {
    herdr_config(true, Some("prefix+a"), SidebarState::Templated)
}

#[test]
fn herdr_check_ok_when_fully_wired() {
    let check = super::herdr_check(&healthy_herdr_probe(), Some(&healthy_herdr_config()));
    assert_eq!(check.health, super::Health::Ok);
    assert!(check.fix.is_none());
    assert!(check.detail.iter().any(|l| l == "herdr: 0.8.0"));
    assert!(
        check
            .detail
            .iter()
            .any(|l| l == "plugin: installed (github)")
    );
    assert!(check.detail.iter().any(|l| l == "key: prefix+a"));
    assert!(check.detail.iter().any(|l| l == "sidebar: templated"));
}

#[test]
fn herdr_check_danger_on_registry_warnings() {
    let probe = herdr_probe(
        Some("0.8.0"),
        Some(herdr_entry(true, None, vec!["plugin root is gone"])),
        None,
    );
    let check = super::herdr_check(&probe, Some(&healthy_herdr_config()));
    assert_eq!(check.health, super::Health::Danger);
    assert!(check.detail.iter().any(|l| l == "  plugin root is gone"));
    assert!(
        check.fix.is_none(),
        "a healthy config offers no fix, even on danger"
    );
}

#[test]
fn herdr_check_danger_on_registry_error() {
    let probe = herdr_probe(
        Some("0.8.0"),
        None,
        Some("herdr's plugin list did not parse"),
    );
    let check = super::herdr_check(&probe, Some(&healthy_herdr_config()));
    assert_eq!(check.health, super::Health::Danger);
    assert!(
        check
            .detail
            .iter()
            .any(|l| l == "  herdr's plugin list did not parse")
    );
}

#[test]
fn herdr_check_warns_without_fix_when_not_installed() {
    let check = super::herdr_check(
        &herdr_probe(Some("0.8.0"), None, None),
        Some(&healthy_herdr_config()),
    );
    assert_eq!(check.health, super::Health::Warn);
    assert!(check.detail.iter().any(|l| l == "plugin: not installed"));
    assert!(check.detail.iter().any(|l| l == "  clauth herdr install"));
    assert!(check.fix.is_none());
}

#[test]
fn herdr_check_warns_without_fix_when_disabled() {
    let probe = herdr_probe(Some("0.8.0"), Some(herdr_entry(false, None, vec![])), None);
    let check = super::herdr_check(&probe, Some(&healthy_herdr_config()));
    assert_eq!(check.health, super::Health::Warn);
    assert!(check.detail.iter().any(|l| l == "plugin: disabled"));
    assert!(check.fix.is_none());
}

#[test]
fn herdr_check_warns_without_fix_when_version_too_old() {
    let probe = herdr_probe(
        Some("0.7.0"),
        Some(herdr_entry(true, Some("0.8.0"), vec![])),
        None,
    );
    let check = super::herdr_check(&probe, Some(&healthy_herdr_config()));
    assert_eq!(check.health, super::Health::Warn);
    assert!(
        check
            .detail
            .iter()
            .any(|l| l == "plugin needs herdr 0.8.0 or newer")
    );
    assert!(check.fix.is_none());
}

#[test]
fn herdr_check_warns_without_fix_when_config_does_not_parse() {
    let check = super::herdr_check(
        &healthy_herdr_probe(),
        Some(&herdr_config(false, None, SidebarState::Absent)),
    );
    assert_eq!(check.health, super::Health::Warn);
    assert!(
        check
            .detail
            .iter()
            .any(|l| l == "herdr's config doesn't parse")
    );
    assert!(check.fix.is_none());
}

#[test]
fn herdr_check_warns_and_offers_fix_when_key_unbound() {
    let check = super::herdr_check(
        &healthy_herdr_probe(),
        Some(&herdr_config(true, None, SidebarState::Templated)),
    );
    assert_eq!(check.health, super::Health::Warn);
    assert!(check.detail.iter().any(|l| l == "key: not bound"));
    assert!(matches!(
        &check.fix,
        Some(super::PluginFix::HealHerdrConfig(p)) if p == &std::path::PathBuf::from("/tmp/herdr/config.toml")
    ));
}

#[test]
fn herdr_check_warns_and_offers_fix_when_sidebar_untemplated() {
    let check = super::herdr_check(
        &healthy_herdr_probe(),
        Some(&herdr_config(true, Some("prefix+a"), SidebarState::Absent)),
    );
    assert_eq!(check.health, super::Health::Warn);
    assert!(check.detail.iter().any(|l| l == "sidebar: not templated"));
    assert!(check.fix.is_some());
}

#[test]
fn herdr_check_warns_without_fix_when_config_unreadable() {
    let check = super::herdr_check(&healthy_herdr_probe(), None);
    assert_eq!(check.health, super::Health::Warn);
    assert!(
        check
            .detail
            .iter()
            .any(|l| l == "herdr's config can't be read")
    );
    assert!(check.fix.is_none());
}

#[test]
fn version_satisfies_compares_componentwise() {
    assert!(super::version_satisfies(Some("0.10.0"), Some("0.9.0")));
    assert!(!super::version_satisfies(Some("0.9.0"), Some("0.10.0")));
    assert!(super::version_satisfies(Some("0.8.0"), Some("0.8.0")));
    assert!(super::version_satisfies(Some("1.0.0"), Some("1.0")));
    assert!(!super::version_satisfies(Some("0.8.9"), Some("0.9.0")));
}

#[test]
fn version_satisfies_stays_quiet_on_unparseable() {
    assert!(super::version_satisfies(Some("v0.7.0"), Some("0.8.0")));
    assert!(super::version_satisfies(Some("0.8.0"), Some("v0.7.0")));
    assert!(super::version_satisfies(None, Some("0.8.0")));
    assert!(super::version_satisfies(Some("0.8.0"), None));
}

#[test]
fn herdr_row_absent_when_herdr_does_not_resolve() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.plugin.herdr = Some(None);
    super::recompute_plugin_checks(&mut app, false);
    assert!(
        !app.plugin.checks.iter().any(|c| c.label == "herdr"),
        "a resolved-but-absent herdr must not render a row, got {:?}",
        app.plugin
            .checks
            .iter()
            .map(|c| c.label)
            .collect::<Vec<_>>()
    );
}

/// The row reads `source_kind` to decide whether a plugin is a local link, and herdr's own output is the only authority on that spelling. Driving the real captured bytes through the parse and into the check is what stops the row drifting onto a spelling herdr never emits: a hand-built fixture agrees with whatever the reader guessed.
#[test]
fn a_real_linked_payload_reads_as_a_local_link() {
    use crate::herdr::{GITHUB, LINKED, plugin_list_json, registry_entry_from};

    let entry = registry_entry_from(&plugin_list_json(LINKED)).expect("linked");
    let root = entry.plugin_root.clone().expect("root");
    let check = super::herdr_check(
        &crate::herdr::HerdrProbe {
            version: Some("0.8.0".to_string()),
            entry: Some(entry),
            config_path: None,
            error: None,
        },
        None,
    );
    assert!(
        check.detail.iter().any(|l| l == "plugin: linked (local)"),
        "reads as a local link: {:?}",
        check.detail
    );
    assert!(
        check.detail.iter().any(|l| *l == format!("root: {root}")),
        "names the checkout it links: {:?}",
        check.detail
    );

    let entry = registry_entry_from(&plugin_list_json(GITHUB)).expect("github");
    let check = super::herdr_check(
        &crate::herdr::HerdrProbe {
            version: Some("0.8.0".to_string()),
            entry: Some(entry),
            config_path: None,
            error: None,
        },
        None,
    );
    assert!(
        check
            .detail
            .iter()
            .any(|l| l == "plugin: installed (github)"),
        "a github install is not a local link: {:?}",
        check.detail
    );
    assert!(
        !check.detail.iter().any(|l| l.starts_with("root: ")),
        "a github install names no checkout: {:?}",
        check.detail
    );
}

/// herdr's warnings and clauth's own probe errors are prose that happens to carry a colon, and `detail_line` turns the first `": "` of an un-indented line into a key column. Left flush, "manifest unavailable: No such file or directory" renders as a field called `manifest unavailable` and widens the key column for every real field in the row, which is what a live run against a stale link showed.
#[test]
fn herdr_prose_lines_are_indented_so_they_do_not_read_as_fields() {
    use crate::herdr::{STALE, plugin_list_json, registry_entry_from};

    let entry = registry_entry_from(&plugin_list_json(STALE)).expect("stale");
    let warning = entry.warnings.first().cloned().expect("a warning");
    assert!(
        warning.contains(": "),
        "the hazard needs a colon: {warning}"
    );

    let check = super::herdr_check(
        &crate::herdr::HerdrProbe {
            version: Some("0.8.0".to_string()),
            entry: Some(entry),
            config_path: None,
            error: None,
        },
        None,
    );
    assert!(
        check.detail.iter().any(|l| *l == format!("  {warning}")),
        "the warning renders as an indented sub-line: {:?}",
        check.detail
    );

    let check = super::herdr_check(
        &crate::herdr::HerdrProbe {
            version: None,
            entry: None,
            config_path: None,
            error: Some("could not run `herdr plugin list --json`: boom".to_string()),
        },
        None,
    );
    assert!(
        check
            .detail
            .iter()
            .any(|l| *l == "  could not run `herdr plugin list --json`: boom"),
        "the probe error renders as an indented sub-line: {:?}",
        check.detail
    );
}

// ── herdr options (Plugin detail) ────────────────────────────────────────────

/// The herdr detail with its options section: a resolved probe + parsed config
/// verdict cached, the check built from them, focus descended. The knobs start
/// at their shipped defaults.
fn herdr_options_app() -> App {
    let mut app = bare_app();
    app.tab = super::Tab::Plugin;
    app.plugin.herdr = Some(Some(healthy_herdr_probe()));
    app.plugin.herdr_config = Some(healthy_herdr_config());
    app.plugin.checks = vec![super::herdr_check(
        &healthy_herdr_probe(),
        Some(&healthy_herdr_config()),
    )];
    app.plugin.cursor = 0;
    app.plugin.focus = super::PluginFocus::Detail;
    app
}

/// The persisted knobs, through the real load path (`load_config` re-reads
/// profiles.toml) — a key handler that mutates memory without saving reds
/// this, and so does a save that writes a different shape than the loader
/// reads.
fn herdr_knobs() -> crate::profile::HerdrSettings {
    crate::profile::load_config()
        .expect("load persisted knobs")
        .state
        .herdr
}

/// `popup width`: space cycles fit → half → split-right → split-top → fit
/// and ⏎ mirrors it (no separate edit step), persisting each step.
#[test]
fn herdr_popup_width_cycles_and_persists() {
    use super::{KeyCode, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app();
    let space = crate::testutil::key(KeyCode::Char(' '));

    handle_key(&mut app, space);
    assert_eq!(
        herdr_knobs().popup_width,
        crate::profile::PopupWidth::Half,
        "space cycles to the next option"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(
        herdr_knobs().popup_width,
        crate::profile::PopupWidth::SplitRight,
        "⏎ mirrors space on a cycle row"
    );
    handle_key(&mut app, space);
    assert_eq!(
        herdr_knobs().popup_width,
        crate::profile::PopupWidth::SplitTop,
        "space keeps cycling forward"
    );
    handle_key(&mut app, space);
    assert_eq!(
        herdr_knobs().popup_width,
        crate::profile::PopupWidth::Fit,
        "the cycle wraps"
    );
}

/// `pane tag`: space toggles the knob and persists through the real save/load
/// path. The toggle fires the knob push, so the herdr runtime env is pinned
/// (HERDR_ENV dropped, the paths pointed into the sandbox): an ambient herdr
/// environment must never make this test re-report the live panes.
#[test]
fn herdr_pane_tag_toggles_and_persists() {
    use super::{KeyCode, handle_key};
    let home = crate::testutil::HomeSandbox::new();
    let tmp = tempfile::tempdir_in(home.home()).expect("tempdir");
    let _env = HerdrRuntimePin::new(&home, &tmp.path().join("herdr"), tmp.path(), false);
    let mut app = herdr_options_app();
    app.plugin.herdr_options_cursor = 1;
    let space = crate::testutil::key(KeyCode::Char(' '));

    handle_key(&mut app, space);
    assert!(!herdr_knobs().pane_tag, "space flips the default on → off");
    handle_key(&mut app, space);
    assert!(herdr_knobs().pane_tag, "and back");
}

/// `tag refresh`: `+`/`-` step live with a floor of 1, ⏎ opens the typed editor
/// (commits on ⏎, discards on ⎋, invalid input stays in the editor) — the
/// Config-tab refresh-interval mechanism.
#[test]
fn herdr_tag_refresh_steps_types_and_persists() {
    use super::{KeyCode, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app();
    app.plugin.herdr_options_cursor = 2;

    for _ in 0..4 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Char('-')));
    }
    assert_eq!(herdr_knobs().tag_watch_secs, 1, "four steps down from 5");
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('-')));
    assert_eq!(herdr_knobs().tag_watch_secs, 1, "the floor is 1, never 0");
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('+')));
    assert_eq!(herdr_knobs().tag_watch_secs, 2);

    // ⏎ opens the typed editor, seeded with the current value.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        app.plugin.herdr_tag_draft.is_some(),
        "⏎ opens the typed editor"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Backspace));
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('3')));
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('0')));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(herdr_knobs().tag_watch_secs, 30, "the typed value commits");
    assert!(app.plugin.herdr_tag_draft.is_none());

    // An under-floor value keeps the editor open and persists nothing.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    handle_key(&mut app, crate::testutil::key(KeyCode::Backspace));
    handle_key(&mut app, crate::testutil::key(KeyCode::Backspace));
    handle_key(&mut app, crate::testutil::key(KeyCode::Backspace));
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('0')));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        app.plugin.herdr_tag_draft.is_some(),
        "an invalid value stays in the editor"
    );
    assert_eq!(herdr_knobs().tag_watch_secs, 30, "and persists nothing");
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(app.plugin.herdr_tag_draft.is_none(), "⎋ discards the draft");
    assert_eq!(herdr_knobs().tag_watch_secs, 30);
}

/// `border label`: space toggles the default-off knob and persists. The
/// toggle fires the knob push, so the herdr runtime env is pinned (HERDR_ENV
/// dropped, the paths pointed into the sandbox): an ambient herdr environment
/// must never make this test re-report the live panes.
#[test]
fn herdr_border_label_toggles_and_persists() {
    use super::{KeyCode, handle_key};
    let home = crate::testutil::HomeSandbox::new();
    let tmp = tempfile::tempdir_in(home.home()).expect("tempdir");
    let _env = HerdrRuntimePin::new(&home, &tmp.path().join("herdr"), tmp.path(), false);
    let mut app = herdr_options_app();
    app.plugin.herdr_options_cursor = 3;

    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        herdr_knobs().border_label,
        "space flips the default off → on"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(!herdr_knobs().border_label, "and back");
}

/// Toggling a knob `report-profile.sh` reads pushes the change onto every
/// live pane in the same key press: `herdr pane list` enumerates the panes
/// once, then each pane gets one re-run of the reporter with its pane id set
/// and the event/context JSON cleared — the `watch-profile.sh` invocation.
/// The stale event/context values the pin plants in the process env prove the
/// clearing is explicit, not inherited luck.
#[cfg(unix)]
#[test]
fn herdr_border_label_toggle_reruns_the_pane_report_per_pane() {
    use super::{KeyCode, handle_key};
    let home = crate::testutil::HomeSandbox::new();
    let tmp = tempfile::tempdir_in(home.home()).expect("tempdir");
    let herdr_shim = write_shim(
        tmp.path(),
        "herdr",
        "printf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/herdr.log\"\nprintf '%s\\n' '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[{\"pane_id\":\"pane-a\"},{\"pane_id\":\"pane-b\"}]}}'",
    );
    let _report_shim = write_shim(
        tmp.path(),
        "report-profile.sh",
        "set -u\nprintf 'pane=%s event=%s context=%s\\n' \"$HERDR_PANE_ID\" \"$HERDR_PLUGIN_EVENT_JSON\" \"$HERDR_PLUGIN_CONTEXT_JSON\" >> \"$(dirname \"$0\")/report.log\"",
    );
    let _env = HerdrRuntimePin::new(&home, &herdr_shim, tmp.path(), true);
    let mut app = herdr_options_app();
    app.plugin.herdr_options_cursor = 3;

    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        herdr_knobs().border_label,
        "space flips the default off → on"
    );
    super::join_test_workers();

    let herdr_log = std::fs::read_to_string(tmp.path().join("herdr.log")).expect("herdr log");
    assert_eq!(
        herdr_log, "pane list\n",
        "the only herdr call is the pane enumeration"
    );
    let report_log = std::fs::read_to_string(tmp.path().join("report.log")).expect("report log");
    assert_eq!(
        report_log, "pane=pane-a event= context=\npane=pane-b event= context=\n",
        "one re-report per listed pane, its pane id set and the event/context JSON cleared"
    );
}

/// The same toggle with no `HERDR_ENV` — a standalone TUI has no panes to
/// reach — spawns nothing at all: the shims' logs never appear even though
/// the binary and plugin-root paths are pinned. The knob itself still
/// toggles and persists.
#[cfg(unix)]
#[test]
fn herdr_border_label_toggle_spawns_nothing_outside_herdr() {
    use super::{KeyCode, handle_key};
    let home = crate::testutil::HomeSandbox::new();
    let tmp = tempfile::tempdir_in(home.home()).expect("tempdir");
    let herdr_shim = write_shim(
        tmp.path(),
        "herdr",
        "printf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/herdr.log\"\nprintf '%s\\n' '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[{\"pane_id\":\"pane-a\"},{\"pane_id\":\"pane-b\"}]}}'",
    );
    let _report_shim = write_shim(
        tmp.path(),
        "report-profile.sh",
        "set -u\nprintf 'pane=%s event=%s context=%s\\n' \"$HERDR_PANE_ID\" \"$HERDR_PLUGIN_EVENT_JSON\" \"$HERDR_PLUGIN_CONTEXT_JSON\" >> \"$(dirname \"$0\")/report.log\"",
    );
    let _env = HerdrRuntimePin::new(&home, &herdr_shim, tmp.path(), false);
    let mut app = herdr_options_app();
    app.plugin.herdr_options_cursor = 3;

    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        herdr_knobs().border_label,
        "the knob still flips and persists standalone"
    );
    super::join_test_workers();

    assert!(
        !tmp.path().join("herdr.log").exists(),
        "no pane enumeration ran"
    );
    assert!(
        !tmp.path().join("report.log").exists(),
        "no pane re-report ran"
    );
}

/// `delegate dot`: space toggles the default-on knob and persists.
#[test]
fn herdr_delegate_dot_toggles_and_persists() {
    use super::{KeyCode, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app();
    app.plugin.herdr_options_cursor = 4;

    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        !herdr_knobs().delegate_dot,
        "space flips the default on → off"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(herdr_knobs().delegate_dot, "and back");
}

/// `delegate row text` is inert while herdr's config does not parse: space
/// opens no modal, nothing persists, and ↑↓ still walks past the row. The
/// other rows stay live in the same state.
#[test]
fn delegate_row_text_is_inert_when_herdr_config_does_not_parse() {
    use super::{KeyCode, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app();
    app.plugin.herdr_config = Some(herdr_config(false, None, SidebarState::Absent));
    app.plugin.herdr_options_cursor = 5;

    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        app.modals.is_empty(),
        "no confirm while the config does not parse"
    );
    assert!(!herdr_knobs().delegate_row_text);
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        app.plugin.herdr_options_cursor, 0,
        "selection wraps past the inert row"
    );

    app.plugin.herdr_options_cursor = 4;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(!herdr_knobs().delegate_dot, "the other rows stay live");
}

/// A fixture whose herdr probe points at a temp config file, so the confirm
/// flow runs heal against a path the test owns. The base config must already
/// be written: the recompute inside reads it for the cached verdict. Checks
/// come from the real recompute — the herdr check sits at its production
/// index, so the post-confirm recompute inside `run_herdr_heal` leaves the
/// cursor where it was (the real app's shape, not a one-element hand-built
/// list that a recompute would strand on `about`).
fn herdr_options_app_with_config(path: &std::path::Path) -> App {
    let mut app = bare_app();
    app.tab = super::Tab::Plugin;
    let probe = crate::herdr::HerdrProbe {
        version: Some("0.8.0".to_string()),
        entry: Some(herdr_entry(true, Some("0.8.0"), vec![])),
        config_path: Some(path.to_path_buf()),
        error: None,
    };
    app.plugin.herdr = Some(Some(probe));
    app.plugin.focus = super::PluginFocus::Detail;
    super::recompute_plugin_checks(&mut app, false);
    app.plugin.cursor = super::HERDR_SELECTOR_ROW;
    app.plugin.herdr_options_cursor = 5;
    app
}

/// Opening the confirm and canceling it: the reworded copy names the delegate
/// token, cancel is the default selection, and esc leaves the knob unchanged
/// and heal unrun — the config file stays byte-identical.
#[test]
fn delegate_row_text_confirm_copy_and_cancel_leave_everything_alone() {
    use super::{KeyCode, Modal, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_path = tmp.path().join("herdr").join("config.toml");
    std::fs::create_dir_all(config_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&config_path, "# my config\n").expect("write base config");
    let mut app = herdr_options_app_with_config(&config_path);

    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => {
            assert_eq!(
                state.message, "add the delegate token to herdr's sidebar row?",
                "the copy says what the confirm will write"
            );
            assert!(
                state
                    .detail
                    .as_deref()
                    .unwrap_or("")
                    .contains("$clauth_delegate"),
                "the detail names the delegate token: {:?}",
                state.detail
            );
            assert!(!state.choice, "cancel is the default selection");
        }
        other => panic!("expected the confirm modal, got {other:?}"),
    }

    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(app.modals.is_empty(), "esc cancels the confirm");
    assert!(!herdr_knobs().delegate_row_text, "the knob stays off");
    assert_eq!(
        std::fs::read_to_string(&config_path).expect("read config"),
        "# my config\n",
        "cancel runs no heal — the config is byte-identical"
    );
}

/// Write a POSIX shim named `name` whose body runs after the shebang, chmod
/// +x, and return its path — the shape the mcp herdr-report pins use, because
/// `herdr_bin()` resolves HERDR_BIN_PATH at call time.
#[cfg(unix)]
fn write_shim(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write shim");
    let mut perms = std::fs::metadata(&path).expect("stat shim").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod shim");
    path
}

/// RAII pin for `HERDR_BIN_PATH`, restored on drop (even on panic). Borrows
/// the [`crate::testutil::HomeSandbox`]: the env is a process-global
/// serialized by `HOME_TEST_LOCK`, which the sandbox holds.
#[cfg(unix)]
struct HerdrBinPin<'a> {
    prev: Option<std::ffi::OsString>,
    _home: std::marker::PhantomData<&'a crate::testutil::HomeSandbox>,
}

#[cfg(unix)]
impl<'a> HerdrBinPin<'a> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    fn new(_home: &'a crate::testutil::HomeSandbox, bin: &std::path::Path) -> Self {
        let prev = std::env::var_os("HERDR_BIN_PATH");
        unsafe { std::env::set_var("HERDR_BIN_PATH", bin) };
        Self {
            prev,
            _home: std::marker::PhantomData,
        }
    }
}

#[cfg(unix)]
impl Drop for HerdrBinPin<'_> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, restored on drop"
    )]
    fn drop(&mut self) {
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var("HERDR_BIN_PATH", v),
                None => std::env::remove_var("HERDR_BIN_PATH"),
            }
        }
    }
}

/// RAII pin for the vars the knob push reads (`HERDR_ENV`, `HERDR_BIN_PATH`,
/// `HERDR_PLUGIN_ROOT`) plus stale event/context values the per-pane re-run
/// must NOT inherit — the push clears them, so a child carrying "stale-event"
/// reds the pin. With `herdr_env: false` only `HERDR_ENV` is removed: the
/// other vars stay pinned to prove the gate, not a missing path, is what
/// suppresses the spawn. Restored on drop (even on panic). Same contract as
/// [`HerdrBinPin`]: process-global env, serialized by `HOME_TEST_LOCK`, which
/// the borrowed sandbox holds.
struct HerdrRuntimePin<'a> {
    prevs: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _home: std::marker::PhantomData<&'a crate::testutil::HomeSandbox>,
}

impl<'a> HerdrRuntimePin<'a> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, held by the borrowed sandbox"
    )]
    fn new(
        _home: &'a crate::testutil::HomeSandbox,
        bin: &std::path::Path,
        plugin_root: &std::path::Path,
        herdr_env: bool,
    ) -> Self {
        let mut prevs = Vec::new();
        for (key, value) in [
            ("HERDR_ENV", herdr_env.then_some("1".to_string())),
            (
                "HERDR_BIN_PATH",
                Some(bin.as_os_str().to_string_lossy().into_owned()),
            ),
            (
                "HERDR_PLUGIN_ROOT",
                Some(plugin_root.as_os_str().to_string_lossy().into_owned()),
            ),
            ("HERDR_PLUGIN_EVENT_JSON", Some("stale-event".to_string())),
            (
                "HERDR_PLUGIN_CONTEXT_JSON",
                Some("stale-context".to_string()),
            ),
        ] {
            let prev = std::env::var_os(key);
            unsafe {
                match &value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            prevs.push((key, prev));
        }
        Self {
            prevs,
            _home: std::marker::PhantomData,
        }
    }
}

impl Drop for HerdrRuntimePin<'_> {
    #[expect(
        unsafe_code,
        reason = "env mutation is unsafe in Rust 2024; serialized by HOME_TEST_LOCK, restored on drop"
    )]
    fn drop(&mut self) {
        for (key, prev) in self.prevs.iter().rev() {
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// Confirming flips the knob on, persists it FIRST, then runs heal with the
/// new value: herdr's config gains the delegate-token row and the user's own
/// content survives. The heal invocation is pinned through a shim standing in
/// as HERDR_BIN_PATH — `herdr config check` runs twice (before + after the
/// write), and the write only lands once both accept.
#[cfg(unix)]
#[test]
fn delegate_row_text_confirm_persists_the_knob_then_heals() {
    use super::{KeyCode, Modal, handle_key};
    let home = crate::testutil::HomeSandbox::new();
    let tmp = tempfile::tempdir_in(home.home()).expect("tempdir");
    let config_path = tmp.path().join("herdr").join("config.toml");
    std::fs::create_dir_all(config_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&config_path, "# my config\n").expect("write base config");
    let mut app = herdr_options_app_with_config(&config_path);
    let shim = write_shim(
        tmp.path(),
        "herdr-shim",
        "echo \"$@\" >> \"$(dirname \"$0\")/heal.log\"",
    );
    let _bin = HerdrBinPin::new(&home, &shim);

    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        matches!(app.modals.last(), Some(Modal::Confirm(_))),
        "space opens the confirm"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Right));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));

    assert!(
        herdr_knobs().delegate_row_text,
        "confirm persists the knob through the real save path"
    );
    let text = std::fs::read_to_string(&config_path).expect("read config");
    assert!(
        text.starts_with("# my config\n"),
        "the user's own content survives the heal: {text}"
    );
    assert!(
        text.contains("$clauth_delegate"),
        "heal wrote the row the new knob asks for: {text}"
    );
    assert_eq!(
        text.matches("rows_by_agent").count(),
        1,
        "exactly one sidebar row: {text}"
    );
    let log = std::fs::read_to_string(tmp.path().join("heal.log")).expect("shim log");
    assert_eq!(
        log.lines().filter(|l| *l == "config check").count(),
        2,
        "heal validates before and after the write: {log}"
    );
}

/// The same flow back: with the knob on, confirming drops the delegate token
/// from the row clauth wrote — the direction-aware copy names the write.
#[cfg(unix)]
#[test]
fn delegate_row_text_confirm_turns_the_knob_back_off() {
    use super::{KeyCode, Modal, handle_key};
    let home = crate::testutil::HomeSandbox::new();
    let tmp = tempfile::tempdir_in(home.home()).expect("tempdir");
    let config_path = tmp.path().join("herdr").join("config.toml");
    std::fs::create_dir_all(config_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&config_path, "# my config\n").expect("write base config");
    let mut app = herdr_options_app_with_config(&config_path);
    let shim = write_shim(
        tmp.path(),
        "herdr-shim",
        "echo \"$@\" >> \"$(dirname \"$0\")/heal.log\"",
    );
    let _bin = HerdrBinPin::new(&home, &shim);

    // First turn it on, the way the test above pins it.
    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    handle_key(&mut app, crate::testutil::key(KeyCode::Right));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(herdr_knobs().delegate_row_text);

    // Now the off direction: the copy names what it will write out.
    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => {
            assert_eq!(
                state.message, "drop the delegate token from herdr's sidebar row?",
                "the off-direction copy says what it writes"
            );
        }
        other => panic!("expected the confirm modal, got {other:?}"),
    }
    handle_key(&mut app, crate::testutil::key(KeyCode::Right));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));

    assert!(!herdr_knobs().delegate_row_text, "the knob turns back off");
    let text = std::fs::read_to_string(&config_path).expect("read config");
    assert!(
        !text.contains("$clauth_delegate"),
        "heal rewrote the row without the token: {text}"
    );
    assert!(
        text.contains("$clauth"),
        "the row itself stays, minus the delegate token: {text}"
    );
}

// ── herdr mode landing ───────────────────────────────────────────────────────

/// `with_herdr_mode(true)` lands on the Plugin tab with the herdr selector
/// row under the cursor, checks already recomputed so the first paint is not
/// empty. Construction probes herdr right away — `HERDR_ENV=1` proves herdr
/// is present — so on a real run the row is there at first paint; the probe
/// is skipped under test (it would read the real registry), and the
/// injected-probe half below pins the landed cursor. The `claude --version`
/// probe stays `r`-gated: construction must not block the first paint on a
/// spawn.
#[test]
fn herdr_mode_lands_on_the_plugin_tab_with_the_herdr_row_selected() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app().with_herdr_mode(true);

    assert_eq!(app.tab, super::Tab::Plugin, "herdr mode opens on Plugin");
    assert!(app.herdr_mode);
    assert!(
        matches!(app.plugin.focus, super::PluginFocus::List),
        "the landing must not steal focus into the detail pane"
    );
    assert!(
        matches!(app.plugin.herdr, Some(None)),
        "construction ran the probe (skipped under test, standing in as no herdr)"
    );
    assert!(
        app.plugin.cc_version.is_none(),
        "construction must not spawn `claude --version`; the probe stays `r`-gated"
    );
    let labels: Vec<&str> = app.plugin.checks.iter().map(|c| c.label).collect();
    assert_eq!(
        labels,
        vec!["about", "mcp servers", "plugin", "runtime"],
        "construction recomputes the checks, so the first paint is not empty"
    );
    assert_eq!(app.plugin.cursor, 3, "the landing cursor is the herdr slot");
    assert_eq!(
        app.plugin.selected_check().map(|c| c.label),
        Some("runtime"),
        "with no herdr resolved the same index rests on the last row"
    );
    // Unprobed must read as unprobed, never as a missing binary.
    let about = &app.plugin.checks[0];
    assert_eq!(about.label, "about");
    assert!(
        about.detail.iter().any(|l| l == "claude: press r to probe"),
        "the about row invites the `r` probe: {:?}",
        about.detail
    );
    assert!(
        !about.detail.iter().any(|l| l == "claude: not found"),
        "an unprobed version must not claim claude is missing: {:?}",
        about.detail
    );

    // The probe resolves (a real construction runs it, `r` re-runs it): the
    // herdr row inserts at the landing index and the cursor is on it without
    // any key handling.
    app.plugin.herdr = Some(Some(healthy_herdr_probe()));
    super::recompute_plugin_checks(&mut app, false);
    assert_eq!(app.plugin.cursor, 3);
    assert_eq!(
        app.plugin.selected_check().map(|c| c.label),
        Some("herdr"),
        "the landing row is the herdr check once it renders"
    );

    // `r` is still the only thing that probes the version.
    super::recompute_plugin_checks(&mut app, true);
    assert!(
        app.plugin.cc_version.is_some(),
        "`r` runs the version probe"
    );
}

/// The mode-less constructor is untouched: Overview, first row, no flag.
#[test]
fn a_plain_app_lands_on_overview_with_the_first_row_selected() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = bare_app();
    assert_eq!(app.tab, super::Tab::Overview);
    assert!(!app.herdr_mode);
    assert_eq!(app.plugin.cursor, 0);
    assert!(
        app.plugin.checks.is_empty(),
        "no construction recompute outside herdr mode"
    );
}
