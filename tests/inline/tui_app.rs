#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::lockorder::RankedMutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::usage::{ActivityStore, ProfileActivity, any_busy};

fn make_activity(entries: &[(&str, ProfileActivity)]) -> ActivityStore {
    let store = Arc::new(RankedMutex::new(HashMap::new()));
    for (name, activity) in entries {
        crate::usage::mark_activity(&store, &crate::profile::ProfileName::from(*name), *activity);
    }
    store
}

fn bootstrap_busy(flag: &Arc<AtomicBool>, activity: &ActivityStore) -> bool {
    flag.load(Ordering::SeqCst) || any_busy(activity)
}

use super::InputState;
use crate::fallback::parse_threshold;

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

/// A rotation result reaches the UI thread a tick or more after its worker
/// returned. Clearing the whole profile there drops an OAuth refetch spinner the
/// rotation never raised, so the drain retires the rotation marker alone.
#[test]
fn a_rotation_result_keeps_a_later_oauth_refetch_spinner() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("alice");

    let mut app = bare_app();
    crate::usage::mark_activity(&app.activity, &name, ProfileActivity::Refreshing);
    // the scheduler re-opens the OAuth leg before the UI drains the result.
    crate::usage::mark_activity(&app.activity, &name, ProfileActivity::Fetching);
    app.op_sender
        .send(crate::usage::OpResult {
            name: "alice".to_string(),
            outcome: Ok(()),
        })
        .expect("send op result");
    super::drain_op_results(&mut app);
    assert!(
        !crate::usage::is_idle(&app.activity, &name),
        "the refetch spinner outlives the rotation result it did not belong to"
    );

    // Control: with no later refetch the drain leaves the profile idle, so the
    // assert above cannot pass on a drain that clears nothing at all.
    let mut app = bare_app();
    crate::usage::mark_activity(&app.activity, &name, ProfileActivity::Refreshing);
    app.op_sender
        .send(crate::usage::OpResult {
            name: "alice".to_string(),
            outcome: Ok(()),
        })
        .expect("send op result");
    super::drain_op_results(&mut app);
    assert!(
        crate::usage::is_idle(&app.activity, &name),
        "the rotation marker itself retires on its own result"
    );

    // A switch gate opened after the rotation belongs to the gate's own drain.
    let mut app = bare_app();
    crate::usage::mark_activity(&app.activity, &name, ProfileActivity::Switching);
    app.op_sender
        .send(crate::usage::OpResult {
            name: "alice".to_string(),
            outcome: Ok(()),
        })
        .expect("send op result");
    super::drain_op_results(&mut app);
    assert!(
        !crate::usage::is_idle(&app.activity, &name),
        "a pending switch outlives an unrelated rotation result"
    );
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

/// Seed CC's plugin registry with one clauth install record at `scope`; the
/// record carries `version` only when one is given, since the registry's
/// version field is optional.
fn write_plugin_install(scope: &str, version: Option<&str>) {
    let path = crate::profile::claude_dir()
        .expect("claude dir")
        .join("plugins")
        .join("installed_plugins.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let mut record = serde_json::json!({ "scope": scope });
    if let Some(version) = version {
        record["version"] = serde_json::Value::from(version);
    }
    let body = serde_json::json!({ "plugins": { "clauth@clauth": [record] } });
    std::fs::write(&path, serde_json::to_vec(&body).expect("serialize")).expect("write");
}

#[cfg(unix)]
fn plugin_check(app: &App) -> &super::Check {
    app.services
        .checks
        .iter()
        .find(|c| c.label == "plugin")
        .expect("plugin check present")
}

/// The delegates detail's rows arrive BANDED, from the store.
///
/// `recompute_services_checks` is the detail's only reader, and it must call
/// `jobs::list_banded` — the same function `clauth jobs` and `monitor`'s listing
/// call — rather than `jobs::list`. The renderer sorts nothing any more, so this
/// read is the whole of the detail's ordering: reverting it to the raw retention
/// order silently drops a long-running delegate below a burst of completions,
/// which is the defect the shared function exists to prevent, and no render test
/// would catch it because they all feed their fixtures in directly.
///
/// Cross-band on purpose, and the finished rows are anchored NEWER than the live
/// one, so raw retention order and banded order disagree.
#[test]
fn the_delegates_detail_reads_the_store_in_banded_order() {
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
                provider: None,
                isolated: false,
                cwd: None,
                spawned_by: None,
                host_session: None,
                host_pid: 0,
                idle_secs: Some(300),
                kind: jobs::RecordKind::Collectable,
                owner_pid: 0,
                owner_started_at: 0,
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
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);

    let ids: Vec<String> = app
        .services
        .delegates
        .iter()
        .map(|j| j.record.job_id.clone())
        .collect();
    let phases: Vec<JobPhase> = app.services.delegates.iter().map(|j| j.phase()).collect();

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

/// The folded plugin check's health is the worst of its four sub-checks (clauth
/// on PATH, mcpServers wiring, plugin install, CC version). Driven through the
/// pure `plugin_check` with a controlled PATH so each arm is deterministic.
#[test]
fn the_plugin_check_health_is_the_worst_of_the_four() {
    use crate::plugin_probe::McpProbe;
    let _home = crate::testutil::HomeSandbox::new();
    let path = std::path::PathBuf::from("/usr/bin/clauth");

    // clauth missing alone is danger, whatever the others say.
    let check = super::plugin_check(None, Some(Some("1.2.3".to_string())), Some(McpProbe::Ok));
    assert_eq!(check.health, super::Health::Danger);

    // clauth present, nothing wired or installed: both fixes warn, and the
    // install leads.
    let check = super::plugin_check(Some(&path), None, None);
    assert_eq!(check.health, super::Health::Warn);
    assert_eq!(check.fix, Some(super::ServiceFix::InstallPlugin));

    // A user-scope install wires the server and installs globally: ok, no fix.
    write_plugin_install("user", Some(env!("CARGO_PKG_VERSION")));
    let check = super::plugin_check(
        Some(&path),
        Some(Some("1.2.3".to_string())),
        Some(McpProbe::Ok),
    );
    assert_eq!(check.health, super::Health::Ok);
    assert!(check.fix.is_none());

    // A boot failure is danger even when the wiring and install are fine.
    let check = super::plugin_check(
        Some(&path),
        Some(Some("1.2.3".to_string())),
        Some(McpProbe::Failed("refused".to_string())),
    );
    assert_eq!(check.health, super::Health::Danger);
}

/// The folded plugin check carries every readout line in cloudy's order (the
/// install block, the mcp entry / server / source, then claude, path, data),
/// pinned by equality against the production builder — the readouts plus the
/// blank separators and the plain explanation line above each fix. Health is
/// the worst, the problems walk in the same order, and the list `f` is the
/// first problem's fix: the install.
#[test]
fn the_plugin_check_folds_the_readouts_and_lists_problems_in_order() {
    use crate::plugin_probe::McpProbe;
    let _home = crate::testutil::HomeSandbox::new();
    let path = std::path::PathBuf::from("/usr/bin/clauth");
    write_plugin_install("local", Some("0.1.0"));

    let check = super::plugin_check(
        Some(&path),
        Some(Some("1.2.3".to_string())),
        Some(McpProbe::Ok),
    );

    let data_line = format!(
        "data: {}",
        crate::profile::clauth_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "\u{2014}".to_string())
    );
    let expected = vec![
        "installed: yes (local)".to_string(),
        "version: 0.1.0".to_string(),
        "installed for this project only, not global".to_string(),
        "installs at user scope".to_string(),
        "f  install plugin".to_string(),
        String::new(),
        "mcp entry: registered".to_string(),
        "mcp server: ok".to_string(),
        "mcp source: plugin install (project)".to_string(),
        "registered for this project only, not global".to_string(),
        "writes the clauth entry into ~/.claude.json".to_string(),
        "f  wire mcp server".to_string(),
        String::new(),
        "claude: 1.2.3".to_string(),
        "path: /usr/bin/clauth".to_string(),
        data_line,
    ];
    assert_eq!(
        check.detail, expected,
        "the folded detail, its separators and its fix explanations, by equality"
    );
    assert_eq!(check.health, super::Health::Warn, "worst-of-four is warn");
    assert_eq!(
        check.fix,
        Some(super::ServiceFix::InstallPlugin),
        "list `f` fixes the first fixable problem shown: the install"
    );
    assert_eq!(
        check
            .problems
            .iter()
            .map(|p| (p.line, super::fix_verb(&p.fix)))
            .collect::<Vec<_>>(),
        vec![(4, "install plugin"), (11, "wire mcp server")],
        "the problems carry their detail-line indices and walk in order"
    );
    assert!(
        !check.detail.iter().any(|l| l.starts_with("[f]")),
        "the bracketed `[f]` anti-pattern is gone: {:?}",
        check.detail
    );
}

/// A manual global `~/.claude.json` wire reads `mcp entry: registered` and
/// leaves no wire fix; the only remaining problem is the (absent) install.
#[test]
fn the_plugin_check_reads_a_global_wire_as_wired() {
    use crate::plugin_probe::McpProbe;
    let _home = crate::testutil::HomeSandbox::new();
    crate::plugin_probe::wire_mcp_server().expect("wire ~/.claude.json");
    let check = super::plugin_check(
        Some(std::path::Path::new("/usr/bin/clauth")),
        Some(Some("1.2.3".to_string())),
        Some(McpProbe::Ok),
    );
    assert!(
        check.detail.iter().any(|l| l == "mcp entry: registered"),
        "a globally wired server reads registered: {:?}",
        check.detail
    );
    assert!(
        check
            .detail
            .iter()
            .any(|l| l == "mcp source: ~/.claude.json (manual)"),
        "and the source names the manual global config: {:?}",
        check.detail
    );
    assert!(
        check
            .problems
            .iter()
            .all(|p| p.fix != super::ServiceFix::WireMcpServers),
        "no wire fix when globally wired"
    );
}

/// A user-scope install wires the server globally and installs the plugin:
/// healthy, no wire fix, no install fix.
#[test]
fn the_plugin_check_reads_a_user_install_as_healthy() {
    use crate::plugin_probe::McpProbe;
    let _home = crate::testutil::HomeSandbox::new();
    write_plugin_install("user", Some(env!("CARGO_PKG_VERSION")));
    let check = super::plugin_check(
        Some(std::path::Path::new("/usr/bin/clauth")),
        Some(Some("1.2.3".to_string())),
        Some(McpProbe::Ok),
    );
    assert_eq!(check.health, super::Health::Ok);
    assert!(
        check.detail.iter().any(|l| l == "installed: yes (user)"),
        "the install readout names the user scope: {:?}",
        check.detail
    );
    assert!(
        check.problems.is_empty(),
        "no problems on a healthy global install"
    );
    assert!(check.fix.is_none());
}

/// Claude Code caches the plugin by its manifest version, so a user install
/// older than this clauth keeps serving the old tree until something
/// reinstalls it. The version line names both versions, the row warns, and the
/// install fix (which updates it) closes the install group with no
/// explanation line. Pinned whole, by equality. `0.9.0` sorts after `0.17.0`
/// as text, so at that version only a numeric comparison reads it older (the
/// comparison itself is pinned in `tests/inline/update.rs`).
#[test]
fn the_plugin_check_warns_on_a_user_install_older_than_this_clauth() {
    use crate::plugin_probe::McpProbe;
    let _home = crate::testutil::HomeSandbox::new();
    write_plugin_install("user", Some("0.9.0"));
    let check = super::plugin_check(
        Some(std::path::Path::new("/usr/bin/clauth")),
        Some(Some("1.2.3".to_string())),
        Some(McpProbe::Ok),
    );
    let data_line = format!(
        "data: {}",
        crate::profile::clauth_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "\u{2014}".to_string())
    );
    assert_eq!(
        check.detail,
        vec![
            "installed: yes (user)".to_string(),
            format!(
                "version: 0.9.0 (this clauth is {})",
                env!("CARGO_PKG_VERSION")
            ),
            "f  install plugin".to_string(),
            String::new(),
            "mcp entry: registered".to_string(),
            "mcp server: ok".to_string(),
            "mcp source: plugin install (user)".to_string(),
            String::new(),
            "claude: 1.2.3".to_string(),
            "path: /usr/bin/clauth".to_string(),
            data_line,
        ],
        "an outdated install names both versions and offers the install, by equality"
    );
    assert_eq!(
        check.health,
        super::Health::Warn,
        "an outdated install warns"
    );
    assert_eq!(
        check.fix,
        Some(super::ServiceFix::InstallPlugin),
        "list `f` updates the outdated install"
    );
    assert_eq!(
        check
            .problems
            .iter()
            .map(|p| (p.line, super::fix_verb(&p.fix)))
            .collect::<Vec<_>>(),
        vec![(2, "install plugin")],
        "the install fix is the one problem, on its own `f` line"
    );
}

/// Only an OLDER user install warns. One at this clauth's own version is
/// current; a newer one is a newer clauth's install, which agentgear never
/// downgrades, so the fix could change nothing; a missing or unparseable
/// version proves nothing either way. Each keeps the healthy row and a plain
/// version line.
#[test]
fn the_plugin_check_reads_a_user_install_that_is_not_older_as_healthy() {
    use crate::plugin_probe::McpProbe;
    let cases = [
        (
            Some(env!("CARGO_PKG_VERSION")),
            Some(format!("version: {}", env!("CARGO_PKG_VERSION"))),
            "this clauth's own version",
        ),
        (
            Some("999.0.0"),
            Some("version: 999.0.0".to_string()),
            "a newer clauth's install",
        ),
        (
            Some("dev"),
            Some("version: dev".to_string()),
            "an unparseable version",
        ),
        (None, None, "a record with no version"),
    ];
    for (version, version_line, case) in cases {
        let _home = crate::testutil::HomeSandbox::new();
        write_plugin_install("user", version);
        let check = super::plugin_check(
            Some(std::path::Path::new("/usr/bin/clauth")),
            Some(Some("1.2.3".to_string())),
            Some(McpProbe::Ok),
        );
        assert_eq!(
            check.health,
            super::Health::Ok,
            "{case}: stays healthy: {:?}",
            check.detail
        );
        assert!(
            check.problems.is_empty() && check.fix.is_none(),
            "{case}: offers no fix: {:?}",
            check.detail
        );
        assert_eq!(
            check
                .detail
                .iter()
                .find(|l| l.starts_with("version"))
                .cloned(),
            version_line,
            "{case}: the version line reads plain"
        );
    }
}

/// The builder's fresh-box plugin detail (nothing installed, no `mcpServers`
/// entry, nothing probed yet), whole: the three groups cloudy picked, each fix
/// under its field.
#[test]
fn the_plugin_check_reads_a_fresh_box_in_three_groups() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = super::plugin_check(Some(std::path::Path::new("/usr/bin/clauth")), None, None);
    let data_line = format!(
        "data: {}",
        crate::profile::clauth_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "\u{2014}".to_string())
    );
    assert_eq!(
        check.detail,
        vec![
            "installed: no (marketplace unknown)".to_string(),
            "installs at user scope".to_string(),
            "f  install plugin".to_string(),
            String::new(),
            "mcp entry: not registered".to_string(),
            "mcp source: none".to_string(),
            "writes the clauth entry into ~/.claude.json".to_string(),
            "f  wire mcp server".to_string(),
            String::new(),
            format!(
                "claude: press {} to probe",
                crate::tui::render::prose::key("r")
            ),
            "path: /usr/bin/clauth".to_string(),
            data_line,
        ],
        "the fresh-box detail, by equality"
    );
    assert_eq!(
        check.problems.iter().map(|p| p.line).collect::<Vec<_>>(),
        vec![2, 7],
        "each fix line's index"
    );
}

/// On a fresh box (nothing installed, no `mcpServers` entry anywhere) list-level
/// `f` on the plugin row opens the install confirm, never the manual wire: the
/// install also registers the server, so it is the one fix such a box needs.
#[test]
fn list_f_on_a_fresh_box_opens_the_install_confirm() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    app.services.cursor = app
        .services
        .checks
        .iter()
        .position(|c| c.label == "plugin")
        .expect("plugin row");
    app.services.focus = super::ServicesFocus::List;
    super::handle_key(
        &mut app,
        crate::testutil::key(ratatui::crossterm::event::KeyCode::Char('f')),
    );
    match app.modals.last() {
        Some(super::Modal::Confirm(state)) => assert!(
            matches!(state.on_confirm, super::ConfirmAction::InstallPlugin),
            "list f on a fresh box installs, got {:?}",
            state.on_confirm
        ),
        other => panic!("expected the install confirm, got {other:?}"),
    }
}

/// A recent delegate rate-limit on any profile names itself in the delegates
/// detail; the dot stays dim (green only while a job runs).
#[test]
fn the_delegates_detail_names_rate_limited_traffic() {
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();
    let now_secs = (crate::usage::now_ms() / 1000) as i64;
    let profile = Profile::new("acct".to_string(), None, None);
    // `record_rate_limit` writes through `write_profile_cache`, which persists
    // only for a profile the saved app state names as configured.
    crate::profile::save_app_state(&AppState {
        profiles: vec![profile.name.clone()],
        ..AppState::default()
    })
    .expect("persist app state");
    crate::throughput::record_rate_limit(&profile.name, None, None, now_secs);
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    });
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    let check = app
        .services
        .checks
        .iter()
        .find(|c| c.label == "delegates")
        .expect("delegates row");
    assert_eq!(check.health, super::Health::Idle, "the dot stays dim");
    assert_eq!(
        check.detail,
        vec!["delegate: rate-limited (acct)".to_string()],
        "the detail names the rate-limited accounts"
    );
}

/// A fix landing under a focused problem shrinks the problem set; the recompute
/// clamps the cursor so the surviving problem is the one `f` fixes.
#[test]
fn the_problem_cursor_clamps_when_a_fix_lands_under_focus() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    let check = app
        .services
        .checks
        .iter()
        .find(|c| c.label == "plugin")
        .expect("plugin row");
    assert_eq!(check.problems.len(), 2, "fixture: wire + install problems");
    app.services.cursor = app
        .services
        .checks
        .iter()
        .position(|c| c.label == "plugin")
        .expect("plugin row");
    app.services.focus = super::ServicesFocus::Detail;
    app.services.problem_cursor = 1;
    // Wire the server: the wire problem leaves, the install problem stays.
    crate::plugin_probe::wire_mcp_server().expect("wire ~/.claude.json");
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert_eq!(
        app.services.problem_cursor, 0,
        "the cursor clamps onto the surviving problem"
    );
    assert_eq!(
        app.services.focused_fix(),
        Some(&super::ServiceFix::InstallPlugin),
        "and `f` fixes the surviving install problem"
    );
}

/// ↵ never descends into the delegates detail (it binds no key).
#[test]
fn enter_does_not_descend_into_the_delegates_detail() {
    use super::{KeyCode, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    app.services.cursor = 1; // delegates is the second row, after shunt
    app.services.focus = super::ServicesFocus::List;
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(
        app.services.focus,
        super::ServicesFocus::List,
        "↵ does not descend into the delegates detail"
    );
}

/// The shunt row's readout: every field that has a value, in cloudy's order
/// (binary, config, version, state, reason, pid, port, restarts, last exit) —
/// `restarts` and `last exit` only when the slot came from the daemon's feed,
/// `reason` split onto indented sub-lines when it spans more than one line.
#[test]
fn shunt_check_reads_the_slot_fields_in_order() {
    use crate::daemon::gateway::{ExitReport, GatewaySlot, GatewayState};
    let every = GatewaySlot {
        state: GatewayState::Healthy,
        config: Some("/home/u/.clauth/gateway.toml".to_string()),
        binary: Some("/usr/local/bin/shunt".to_string()),
        port: Some(3001),
        pid: Some(4242),
        version: Some("0.49.1".to_string()),
        answerer: None,
        floor: "0.48.0".to_string(),
        restarts: 2,
        last_exit: Some(ExitReport {
            code: Some(1),
            signal: None,
        }),
        reason: Some("TOML parse error at line 1\n  |\n1 | foo =".to_string()),
        since: Some("2026-09-29T00:00:00Z".to_string()),
    };
    let check = super::shunt_check(&every, true, None);
    assert_eq!(check.health, super::Health::Ok, "healthy is green");
    assert_eq!(
        check.detail,
        vec![
            "binary: /usr/local/bin/shunt".to_string(),
            "config: /home/u/.clauth/gateway.toml".to_string(),
            "version: 0.49.1".to_string(),
            "state: healthy".to_string(),
            "reason: TOML parse error at line 1".to_string(),
            "    |".to_string(),
            "  1 | foo =".to_string(),
            "pid: 4242".to_string(),
            "port: 3001".to_string(),
            "restarts: 2".to_string(),
            "last exit: exit 1".to_string(),
        ],
        "every set field renders in order, a multi-line reason splits"
    );

    let absent = GatewaySlot {
        state: GatewayState::Absent,
        config: None,
        binary: None,
        port: None,
        pid: None,
        version: None,
        answerer: None,
        floor: "0.48.0".to_string(),
        restarts: 0,
        last_exit: None,
        reason: None,
        since: None,
    };
    let check = super::shunt_check(&absent, false, None);
    assert_eq!(check.health, super::Health::Idle, "absent is dim");
    assert_eq!(
        check.detail,
        vec!["state: not adopted".to_string()],
        "an unprobed absent slot renders state alone, no unobserved figures"
    );
}

/// With no gateway adopted, the row names the config shunt would load and the
/// standalone shunt answering, each only when there is one; an adopted
/// gateway's row never shows a standalone readout.
#[test]
fn an_unadopted_shunt_row_names_what_runs_without_clauth() {
    use crate::daemon::gateway::{GatewaySlot, GatewayState};
    use std::net::SocketAddr;
    let slot = |state| GatewaySlot {
        state,
        config: None,
        binary: None,
        port: None,
        pid: None,
        version: None,
        answerer: None,
        floor: "0.48.0".to_string(),
        restarts: 0,
        last_exit: None,
        reason: None,
        since: None,
    };
    let addr: SocketAddr = "127.0.0.1:3067".parse().unwrap();
    let found = std::path::PathBuf::from("/home/u/.config/shunt/shunt.toml");
    let detail = |state, readout: &super::StandaloneShunt| {
        super::shunt_check(&slot(state), false, Some(readout)).detail
    };

    assert_eq!(
        detail(GatewayState::Absent, &super::StandaloneShunt::default()),
        vec!["state: not adopted".to_string()],
        "nothing found and nothing answering adds nothing",
    );
    assert_eq!(
        detail(
            GatewayState::Absent,
            &super::StandaloneShunt {
                found: Some(found.clone()),
                unread_bind: None,
                unread_config: None,
                unread_env: false,
                answer: None,
            }
        ),
        vec![
            "state: not adopted".to_string(),
            "found: /home/u/.config/shunt/shunt.toml".to_string(),
        ],
        "a config with nothing answering names the config alone",
    );
    let both = super::StandaloneShunt {
        found: Some(found),
        unread_bind: None,
        unread_config: None,
        unread_env: false,
        answer: Some(("0.49.1".to_string(), addr)),
    };
    assert_eq!(
        detail(GatewayState::Absent, &both),
        vec![
            "state: not adopted".to_string(),
            "found: /home/u/.config/shunt/shunt.toml".to_string(),
            "standalone shunt 0.49.1 answers on 127.0.0.1:3067".to_string(),
        ],
    );
    assert_eq!(
        detail(
            GatewayState::Absent,
            &super::StandaloneShunt {
                found: None,
                unread_bind: None,
                unread_config: None,
                unread_env: false,
                answer: Some(("0.49.1\u{1b}[2J".to_string(), addr)),
            }
        ),
        vec![
            "state: not adopted".to_string(),
            "standalone shunt 0.49.1\\u{1b}[2J answers on 127.0.0.1:3067".to_string(),
        ],
        "an answerer with no config found, its version escaped like every slot string",
    );
    let split = detail(
        GatewayState::Absent,
        &super::StandaloneShunt {
            found: None,
            unread_bind: None,
            unread_config: None,
            unread_env: false,
            answer: Some(("0.49.1: forged".to_string(), addr)),
        },
    );
    assert_eq!(
        split[1], "standalone shunt 0.49.1\\u{3a} forged answers on 127.0.0.1:3067",
        "a `: ` in the version is escaped, so the line stays prose",
    );
    assert_eq!(split[1].split_once(": "), None);
    let trailing = detail(
        GatewayState::Absent,
        &super::StandaloneShunt {
            found: None,
            unread_bind: None,
            unread_config: None,
            unread_env: false,
            answer: Some(("0.49.1:".to_string(), addr)),
        },
    );
    assert_eq!(
        trailing[1], "standalone shunt 0.49.1\\u{3a} answers on 127.0.0.1:3067",
        "a trailing `:` would meet the space after it",
    );
    assert_eq!(
        detail(
            GatewayState::Absent,
            &super::StandaloneShunt {
                found: Some(std::path::PathBuf::from("/w/\u{202e}x/shunt.toml")),
                unread_bind: None,
                unread_config: None,
                unread_env: false,
                answer: None,
            }
        ),
        vec![
            "state: not adopted".to_string(),
            "found: /w/\\u{202e}x/shunt.toml".to_string(),
        ],
        "a found path is escaped too: a directory name can carry a bidi override",
    );
    // A YAML, unreadable or unparsed config is unread too, as the readout
    // reports it: the bind line alone names it, never a second `config:` line.
    let unread = |reason: super::UnreadBind| super::StandaloneShunt {
        found: Some(std::path::PathBuf::from("/home/u/.config/shunt/shunt.yaml")),
        unread_config: match reason {
            super::UnreadBind::Yaml => Some(super::UnreadConfig::Yaml),
            super::UnreadBind::Unreadable => Some(super::UnreadConfig::Unreadable),
            super::UnreadBind::Unparsed => Some(super::UnreadConfig::Unparsed),
            super::UnreadBind::Value(_) => None,
        },
        unread_env: false,
        unread_bind: Some(reason),
        answer: Some(("0.49.1".to_string(), addr)),
    };
    assert_eq!(
        detail(GatewayState::Absent, &unread(super::UnreadBind::Yaml)),
        vec![
            "state: not adopted".to_string(),
            "found: /home/u/.config/shunt/shunt.yaml".to_string(),
            "bind: not read (yaml)".to_string(),
            "standalone shunt 0.49.1 answers on 127.0.0.1:3067".to_string(),
        ],
        "the unread bind sits between the config and the answer",
    );
    for (reason, shown) in [
        (
            super::UnreadBind::Unreadable,
            "bind: not read (config unreadable)",
        ),
        (
            super::UnreadBind::Unparsed,
            "bind: not read (config does not parse)",
        ),
        (
            super::UnreadBind::Value("${SHUNT_\u{1b}BIND}".to_string()),
            "bind: not read (${SHUNT_\\u{1b}BIND})",
        ),
        (
            super::UnreadBind::Value(String::new()),
            "bind: not read (empty)",
        ),
    ] {
        assert_eq!(
            detail(GatewayState::Absent, &unread(reason)),
            vec![
                "state: not adopted".to_string(),
                "found: /home/u/.config/shunt/shunt.yaml".to_string(),
                shown.to_string(),
                "standalone shunt 0.49.1 answers on 127.0.0.1:3067".to_string(),
            ],
            "each reason's words, a raw value escaped, and no second line"
        );
    }
    assert_eq!(
        detail(GatewayState::Disabled, &both),
        vec!["state: disabled".to_string()],
        "an adopted gateway keeps its own readout",
    );
}

/// The standalone readout off a discovery result and one `/health` probe of
/// the bind that config sets, against a real stub answering `/health`.
#[test]
fn the_standalone_readout_probes_the_found_configs_bind() {
    let home = crate::testutil::HomeSandbox::new();
    let (base, seen) = crate::testutil::serve_endpoints(1, |_, _| {
        (200, r#"{"status":"ok","version":"0.49.1"}"#.to_string())
    });
    let stub: std::net::SocketAddr = base.trim_start_matches("http://").parse().unwrap();
    let config = home.home().join("shunt.toml");
    // A wildcard bind, as a served shunt often has: the row names the loopback
    // address the probe reached, never `0.0.0.0`.
    std::fs::write(
        &config,
        format!("[server]\nbind = \"0.0.0.0:{}\"\n", stub.port()),
    )
    .unwrap();

    assert_eq!(
        super::standalone_readout_from(
            Ok(Some(config.clone())),
            None,
            crate::gateway::probe_health
        ),
        super::StandaloneShunt {
            found: Some(config.clone()),
            unread_bind: None,
            unread_config: None,
            unread_env: false,
            answer: Some(("0.49.1".to_string(), stub)),
        },
        "the stub on the config's bind answers as a standalone shunt",
    );
    assert_eq!(seen.join().expect("listener"), ["/health"]);

    // A closed port on the config's bind: found, nothing answering.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    std::fs::write(&config, format!("[server]\nbind = \"{closed}\"\n")).unwrap();
    assert_eq!(
        super::standalone_readout_from(
            Ok(Some(config.clone())),
            None,
            crate::gateway::probe_health
        ),
        super::StandaloneShunt {
            found: Some(config),
            unread_bind: None,
            unread_config: None,
            unread_env: false,
            answer: None,
        },
    );
}

/// Where clauth cannot read a bind, the one probe goes to shunt's default
/// and the readout names why; the env override moves the probe whatever the
/// config's format, as it moves shunt's own. No config and no override is not
/// an unread bind: shunt's default IS its bind.
#[test]
fn the_standalone_readout_without_a_readable_bind_probes_the_default() {
    use super::{UnreadBind, UnreadConfig};
    use std::cell::RefCell;
    let home = crate::testutil::HomeSandbox::new();
    let probed = RefCell::new(Vec::new());
    let silent = |addr| {
        probed.borrow_mut().push(addr);
        Ok(crate::gateway::Health::NotShunt { status: 404 })
    };
    let readout = |found: Option<std::path::PathBuf>,
                   unread: Option<UnreadBind>,
                   unread_config: Option<UnreadConfig>| super::StandaloneShunt {
        found,
        unread_bind: unread,
        unread_config,
        unread_env: false,
        answer: None,
    };
    assert_eq!(
        super::standalone_readout_from(Ok(None), None, silent),
        readout(None, None, None),
    );
    assert_eq!(
        super::standalone_readout_from(Ok(None), Some("127.0.0.1:4100"), silent),
        readout(None, None, None),
    );
    assert_eq!(
        super::standalone_readout_from(Ok(None), Some("nope"), silent),
        readout(
            None,
            Some(UnreadBind::Value("SHUNT_SERVER__BIND=nope".to_string())),
            None,
        ),
        "a refused env override is named with its variable",
    );

    let yaml = std::path::PathBuf::from("/home/u/.config/shunt/shunt.yaml");
    let yaml_err = || Err(crate::gateway::YamlConfig { path: yaml.clone() }.into());
    assert_eq!(
        super::standalone_readout_from(yaml_err(), None, silent),
        readout(
            Some(yaml.clone()),
            Some(UnreadBind::Yaml),
            Some(UnreadConfig::Yaml)
        ),
    );
    assert_eq!(
        super::standalone_readout_from(yaml_err(), Some("127.0.0.1:4200"), silent),
        readout(Some(yaml.clone()), None, Some(UnreadConfig::Yaml)),
        "the env override is read, so a YAML config's own bind does not matter, \
         but the config itself stays unread",
    );

    let config = home.home().join("shunt.toml");
    let toml_case = |text: &str| {
        std::fs::write(&config, text).unwrap();
        super::standalone_readout_from(Ok(Some(config.clone())), None, silent).unread_bind
    };
    assert_eq!(
        toml_case("[server]\nbind = \"${SHUNT_BIND}\"\n"),
        Some(UnreadBind::Value("${SHUNT_BIND}".to_string())),
    );
    assert_eq!(
        toml_case("[server]\nbind = \"127.0.0.1:0\"\n"),
        Some(UnreadBind::Value("127.0.0.1:0".to_string())),
    );
    assert_eq!(
        toml_case("[server]\nbind = 3001\n"),
        Some(UnreadBind::Value("3001".to_string())),
        "a non-string bind is named in its TOML spelling",
    );
    assert_eq!(
        toml_case("[server]\nbind = 3001 # the port alone\n"),
        Some(UnreadBind::Value("3001".to_string())),
        "a trailing comment is not part of the value",
    );
    assert_eq!(
        toml_case("[server]\nbind = \"\"\n"),
        Some(UnreadBind::Value(String::new())),
    );
    assert_eq!(
        toml_case("[server.bind]\nx = 1 # a comment\n"),
        Some(UnreadBind::Value("{ x = 1 }".to_string())),
        "a table-shaped bind is named inline, its comment dropped",
    );
    assert_eq!(toml_case("[server\n"), Some(UnreadBind::Unparsed));
    let dir = home.home().join("a-dir-not-a-file");
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(
        super::standalone_readout_from(Ok(Some(dir.clone())), None, silent),
        readout(
            Some(dir.clone()),
            Some(UnreadBind::Unreadable),
            Some(UnreadConfig::Unreadable)
        ),
    );
    assert_eq!(
        super::standalone_readout_from(Ok(Some(dir.clone())), Some("127.0.0.1:4300"), silent),
        readout(Some(dir), None, Some(UnreadConfig::Unreadable)),
        "an env bind moves the probe off an unreadable config, which stays unread",
    );
    // A TOML file that does not parse stays unread under an env bind too:
    // the env hides only the bind's reason.
    std::fs::write(&config, "[server\n").unwrap();
    assert_eq!(
        super::standalone_readout_from(Ok(Some(config.clone())), Some("127.0.0.1:4400"), silent),
        readout(Some(config.clone()), None, Some(UnreadConfig::Unparsed)),
    );
    assert_eq!(
        super::standalone_readout_from(Ok(None), Some("\"127.0.0.1:4500\""), silent),
        readout(None, None, None),
        "a quoted env value is unwrapped, as shunt's own env layer reads it",
    );

    let want: Vec<std::net::SocketAddr> = [
        "127.0.0.1:3001",
        "127.0.0.1:4100",
        "127.0.0.1:3001",
        "127.0.0.1:3001",
        "127.0.0.1:4200",
        "127.0.0.1:3001",
        "127.0.0.1:3001",
        "127.0.0.1:3001",
        "127.0.0.1:3001",
        "127.0.0.1:3001",
        "127.0.0.1:3001",
        "127.0.0.1:3001",
        "127.0.0.1:3001",
        "127.0.0.1:4300",
        "127.0.0.1:4400",
        "127.0.0.1:4500",
    ]
    .iter()
    .map(|a| a.parse().unwrap())
    .collect();
    assert_eq!(
        *probed.borrow(),
        want,
        "one probe per readout, at the bind clauth reads, else shunt's default"
    );
}

/// The dot buckets the gateway states into their classes: green healthy; amber
/// starting/unhealthy/restarting/stopping; red the refusal states; dim
/// absent/disabled/unobserved.
#[test]
fn shunt_check_maps_each_state_to_its_dot_class() {
    use crate::daemon::gateway::{GatewaySlot, GatewayState};
    let slot = |state| GatewaySlot {
        state,
        config: None,
        binary: None,
        port: None,
        pid: None,
        version: None,
        answerer: None,
        floor: "0.48.0".to_string(),
        restarts: 0,
        last_exit: None,
        reason: None,
        since: None,
    };
    let cases: &[(GatewayState, super::Health)] = &[
        (GatewayState::Healthy, super::Health::Ok),
        (GatewayState::Starting, super::Health::Warn),
        (GatewayState::Unhealthy, super::Health::Warn),
        (GatewayState::Restarting, super::Health::Warn),
        (GatewayState::Stopping, super::Health::Warn),
        (GatewayState::Misconfigured, super::Health::Danger),
        (GatewayState::BinaryMissing, super::Health::Danger),
        (GatewayState::Foreign, super::Health::Danger),
        (GatewayState::YamlRefused, super::Health::Danger),
        (GatewayState::BelowFloor, super::Health::Danger),
        (GatewayState::NoConfig, super::Health::Danger),
        (GatewayState::Absent, super::Health::Idle),
        (GatewayState::Disabled, super::Health::Idle),
        (GatewayState::Unobserved, super::Health::Idle),
    ];
    for (state, want) in cases {
        assert_eq!(
            super::shunt_check(&slot(*state), false, None).health,
            *want,
            "{state:?} dot class"
        );
    }
}

/// The sanitizer escapes exactly the control and bidi-formatting characters,
/// and leaves every other character — backslashes, non-ASCII, quotes, the
/// ellipsis — unchanged.
#[test]
fn escape_control_escapes_only_control_and_bidi_chars() {
    assert_eq!(
        super::escape_control(r"C:\Users\u\.clauth\shunt.toml"),
        r"C:\Users\u\.clauth\shunt.toml",
        "a Windows path renders with its backslashes unchanged"
    );
    assert_eq!(
        super::escape_control("/home/zoë/.cargo/bin/shunt"),
        "/home/zoë/.cargo/bin/shunt",
        "a non-ASCII path renders unchanged"
    );
    assert_eq!(
        super::escape_control(r#"invalid value: string "abc", …"#),
        r#"invalid value: string "abc", …"#,
        "a quoted reason renders unchanged"
    );
    assert_eq!(super::escape_control("\u{1b}"), "\\u{1b}", "ESC escapes");
    assert_eq!(super::escape_control("\u{7}"), "\\u{7}", "BEL escapes");
    assert_eq!(
        super::escape_control("\u{202e}"),
        "\\u{202e}",
        "a bidi override escapes"
    );
}

/// Every slot string the detail renders goes through the sanitizer: a planted
/// control character never reaches the detail raw, whichever field carries it.
#[test]
fn shunt_check_escapes_the_untrusted_slot_strings() {
    use crate::daemon::gateway::{GatewaySlot, GatewayState};
    let slot = GatewaySlot {
        state: GatewayState::Healthy,
        config: Some("/tmp/a\u{1b}b".to_string()),
        binary: Some("/tmp/b\u{202e}c".to_string()),
        port: None,
        pid: None,
        version: Some("0.49.1\u{1b}[2J".to_string()),
        answerer: None,
        floor: "0.48.0".to_string(),
        restarts: 0,
        last_exit: None,
        reason: Some("bad \u{202e} entry".to_string()),
        since: None,
    };
    let check = super::shunt_check(&slot, true, None);
    let find = |prefix: &str| {
        check
            .detail
            .iter()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix} line: {:?}", check.detail))
            .clone()
    };
    assert_eq!(
        find("version:"),
        "version: 0.49.1\\u{1b}[2J",
        "the ESC renders as its visible escape"
    );
    assert!(
        !check.detail.iter().any(|l| l.contains('\u{1b}')),
        "no raw control character reaches the readout: {:?}",
        check.detail
    );
    assert!(
        !check.detail.iter().any(|l| l.contains('\u{202e}')),
        "no raw bidi override reaches the readout: {:?}",
        check.detail
    );
    assert!(find("config:").contains("\\u{1b}"), "config is escaped");
    assert!(find("binary:").contains("\\u{202e}"), "binary is escaped");
    assert!(find("reason:").contains("\\u{202e}"), "reason is escaped");
}

/// A `foreign` slot names what answered on the bind port, beside the reason,
/// so the red dot states its cause.
#[test]
fn shunt_check_renders_the_foreign_answerer() {
    use crate::daemon::gateway::{Answerer, GatewaySlot, GatewayState};
    let foreign = |answerer| GatewaySlot {
        state: GatewayState::Foreign,
        config: None,
        binary: None,
        port: Some(3001),
        pid: None,
        version: None,
        answerer,
        floor: "0.48.0".to_string(),
        restarts: 0,
        last_exit: None,
        reason: Some("the port is held".to_string()),
        since: None,
    };
    let expect = |word: &str| {
        vec![
            "state: foreign".to_string(),
            "reason: the port is held".to_string(),
            format!("answerer: {word}"),
            "port: 3001".to_string(),
        ]
    };
    let check = super::shunt_check(&foreign(Some(Answerer::Shunt)), false, None);
    assert_eq!(
        check.detail,
        expect("shunt"),
        "a shunt-shaped foreign answerer names itself"
    );
    let check = super::shunt_check(&foreign(Some(Answerer::NotShunt)), false, None);
    assert_eq!(
        check.detail,
        expect("not shunt"),
        "a non-shunt HTTP answerer names itself"
    );
    let check = super::shunt_check(&foreign(Some(Answerer::NoAnswer)), false, None);
    assert_eq!(
        check.detail,
        expect("no answer"),
        "a silent answerer names itself"
    );
}

/// The reader: a fresh daemon's `gateway` object is the slot (flagged
/// from-daemon); a stale daemon, an absent key, and an unparseable object all
/// fall back to the record-only verdict (flagged not-from-daemon).
#[test]
fn gateway_slot_reads_a_fresh_feed_and_falls_back_otherwise() {
    use crate::daemon::DaemonHealth;
    use crate::daemon::gateway::GatewayState;
    let _home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let status_path = dir.join("status.json");

    let feed = serde_json::json!({
        "gateway": {
            "state": "healthy",
            "floor": "0.48.0",
            "restarts": 2,
            "version": "0.49.1"
        }
    });
    std::fs::write(&status_path, serde_json::to_vec(&feed).expect("serialize")).expect("write");
    let (slot, supervised) = crate::daemon::gateway_slot(DaemonHealth::Fresh);
    assert!(supervised, "a fresh feed's gateway slot is from the daemon");
    assert_eq!(
        slot.state,
        GatewayState::Healthy,
        "and it is the feed's slot"
    );

    // No gateway key (an older daemon): record-only, not from the daemon.
    std::fs::write(&status_path, b"{}").expect("write");
    let (_, supervised) = crate::daemon::gateway_slot(DaemonHealth::Fresh);
    assert!(
        !supervised,
        "a fresh feed with no gateway key is not the daemon's slot"
    );

    // A gateway object this binary cannot parse (a state word from a newer
    // daemon): record-only, not from the daemon.
    std::fs::write(
        &status_path,
        br#"{"gateway": {"state": "adopting", "floor": "0.48.0", "restarts": 0}}"#,
    )
    .expect("write");
    let (_, supervised) = crate::daemon::gateway_slot(DaemonHealth::Fresh);
    assert!(
        !supervised,
        "an unparseable gateway object falls back, not from the daemon"
    );

    // A stale daemon never reads its feed.
    std::fs::write(&status_path, serde_json::to_vec(&feed).expect("serialize")).expect("write");
    let (_, supervised) = crate::daemon::gateway_slot(DaemonHealth::Stale);
    assert!(!supervised, "a stale daemon's verdict is record-only");
}

/// The "the daemon runs the gateway" note names the daemon only for a record
/// the gateway WOULD run on with no daemon to run it: an `unobserved` record.
/// An absent record shows no note — no gateway would run at all.
#[test]
fn the_shunt_row_names_the_daemon_only_for_an_unobserved_record() {
    use crate::daemon::DaemonHealth;
    let _home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let status_path = dir.join("status.json");

    // No record in the sandbox: the record-only verdict is `absent`.
    std::fs::write(&status_path, b"{}").expect("feed");
    let mut app = bare_app();
    app.daemon_health = DaemonHealth::Fresh;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    let shunt = app
        .services
        .checks
        .iter()
        .find(|c| c.label == "shunt")
        .expect("shunt row");
    assert_eq!(
        shunt.detail,
        vec!["state: not adopted".to_string()],
        "an absent record shows no daemon note"
    );

    // A record whose config exists: `unobserved` — the gateway would run on it.
    let config = dir.join("shunt.toml");
    std::fs::write(&config, "port = 3001\n").expect("config");
    std::fs::write(
        dir.join("gateway.toml"),
        format!("config = {:?}\n", config.to_string_lossy()),
    )
    .expect("record");
    let mut app = bare_app();
    app.daemon_health = DaemonHealth::Fresh;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    let shunt = app
        .services
        .checks
        .iter()
        .find(|c| c.label == "shunt")
        .expect("shunt row");
    assert_eq!(
        shunt.detail,
        vec![
            "binary: shunt".to_string(),
            format!("config: {}", config.to_string_lossy()),
            "state: unobserved".to_string(),
            "the daemon runs the gateway".to_string(),
            String::new(),
            "enabled".to_string(),
            "admin  no [server.admin] table".to_string(),
            "f  add admin table".to_string(),
        ],
        "an unobserved record names the daemon as what would run the gateway"
    );
}

/// The install readout must name the OPERATIVE record, not whichever row sorts
/// first on disk: a stale project/local row ahead of the live `user`-scope row
/// used to print the wrong scope and version beside a verdict computed off the
/// user row.
#[test]
fn plugin_check_names_the_user_scope_record_when_one_exists() {
    let _home = crate::testutil::HomeSandbox::new();
    let path = crate::profile::claude_dir()
        .expect("claude dir")
        .join("plugins")
        .join("installed_plugins.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let body = serde_json::json!({
        "plugins": { "clauth@clauth": [
            { "scope": "local", "version": "0.14.1" },
            { "scope": "user", "version": env!("CARGO_PKG_VERSION") }
        ] }
    });
    std::fs::write(&path, serde_json::to_vec(&body).expect("serialize")).expect("write");

    let check = super::plugin_check(Some(std::path::Path::new("/usr/bin/clauth")), None, None);
    assert_eq!(
        check.health,
        super::Health::Ok,
        "a user-scope row is global: {:?}",
        check.detail
    );
    assert!(
        check
            .detail
            .iter()
            .any(|line| line.starts_with("installed: yes (user)")),
        "the operative user row must name the scope: {:?}",
        check.detail
    );
    assert!(
        check
            .detail
            .iter()
            .any(|line| *line == format!("version: {}", env!("CARGO_PKG_VERSION"))),
        "the live user row's version must win: {:?}",
        check.detail
    );
    assert!(
        !check
            .detail
            .iter()
            .any(|line| line.starts_with("version: 0.14.1")),
        "the stale local row must not name the install: {:?}",
        check.detail
    );
}

// ── the install fix drives agentgear at user scope ────────────────────────

/// The confirm gate every mutating fix owes (tab spec: "confirm modal first,
/// default choice = cancel"): `f` on the plugin row must open a
/// [`ConfirmAction::InstallPlugin`] modal that defaults to cancel and runs
/// nothing until confirmed. The server is already wired so the install is the
/// first fixable problem the list `f` reaches.
#[test]
fn the_install_fix_opens_a_default_cancel_confirm_before_installing() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::plugin_probe::wire_mcp_server().expect("wire ~/.claude.json");
    let mut app = bare_app();
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    let idx = app
        .services
        .checks
        .iter()
        .position(|c| c.label == "plugin")
        .expect("plugin check present");
    app.services.cursor = idx;

    super::apply_service_fix(&mut app);

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
    // The install re-enables a disabled plugin, and the outdated-install warn
    // reaches a disabled one too, so the confirm says where the plugin ends up.
    assert_eq!(
        (state.message.as_str(), state.detail.as_deref()),
        (
            "install the clauth plugin into claude code?",
            Some(
                "runs claude's own plugin installer at user scope and turns the plugin on; your other plugins and settings are untouched."
            ),
        ),
        "the install confirm's copy, by equality"
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
/// materialized tree landing under the hermetic data dir, and the Services tab
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
    assert!(
        check.detail.iter().any(|l| l.starts_with("installed: yes")),
        "the row reflects the user-scope install, got {:?}",
        check.detail
    );
    assert!(
        check.fix.is_none(),
        "a global install leaves no install or wire fix, got {:?}",
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
    assert!(
        check.detail.iter().any(|l| l.starts_with("installed: no")),
        "the row still reads not installed, got {:?}",
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
    // runtime and isn't what this test guards. The per-account day list is NOT
    // here: it lives on the Fallback tab's member card.
    assert_eq!(
        rows,
        [
            ConfigRow::Name,
            ConfigRow::AutoStart,
            ConfigRow::BellThreshold,
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

/// The re-login chain opens fields away from the row it starts on: the cursor
/// moves with each field it opens, so the highlight and the caret stay on one
/// row from the first ⏎ to the last.
#[test]
fn api_relogin_chain_keeps_the_cursor_on_the_field_being_typed() {
    use super::{ConfigRow, config_rows, enter_config_detail, handle_key};
    use crate::profile::{AppConfig, AppState, Profile};
    use ratatui::crossterm::event::KeyCode;
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
    app.tab = super::Tab::Setup;
    app.profile_cursor = 0;
    enter_config_detail(&mut app);
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("api account shows a login row");
    let on_cursor = |app: &App| config_rows(app).get(app.config_action_cursor).copied();

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(
        on_cursor(&app),
        Some(ConfigRow::BaseUrl),
        "re-login opens base url"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('x')));
    assert_eq!(
        on_cursor(&app),
        Some(ConfigRow::BaseUrl),
        "typing stays on base url"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.active),
        Some(ConfigRow::ApiKey),
        "precondition: ⏎ advanced the chain"
    );
    assert_eq!(
        on_cursor(&app),
        Some(ConfigRow::ApiKey),
        "⏎ moves to api key"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.active),
        None,
        "precondition: the chain ended"
    );
    assert_eq!(
        on_cursor(&app),
        Some(ConfigRow::ApiKey),
        "the last field keeps it"
    );
}

/// F1: a Setup re-key persists the new credential but the live key-rejected set
/// must drop the name SAME-FRAME — not on the next tick's reload. The success
/// arm calls `refresh_tokens`, so `key_rejected_names` re-evaluates the current
/// entry fingerprint at once instead of intersecting the stale one.
#[test]
fn rekeying_an_endpoint_drops_the_live_broken_mark_immediately() {
    use super::{ConfigRow, InputState, commit_config_field, enter_config_detail};
    use crate::profile::{AppConfig, AppState, Profile, ProfileName};
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["api"]);

    let api = Profile::new(
        "api".to_string(),
        Some("https://api.z.ai/api/anthropic".to_string()),
        Some("old-key".to_string()),
    );

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("api")],
            ..AppState::default()
        },
        profiles: vec![api],
    });

    // The live rejection mark is recorded under the OLD credential fingerprint.
    let old_fp = app
        .third_party_tokens
        .lock()
        .expect("tokens store unpoisoned")
        .iter()
        .find(|e| e.name.as_str() == "api")
        .expect("the old third-party entry is collected at construction")
        .credential_fingerprint();
    app.third_party_broken
        .lock()
        .expect("broken store unpoisoned")
        .insert("api".to_string(), old_fp);
    assert!(
        app.key_rejected_names().contains(&ProfileName::from("api")),
        "the old key's rejection is live before the re-key"
    );

    // Re-key through the real producer (the Setup endpoint editor's ⏎).
    app.profile_cursor = 0;
    enter_config_detail(&mut app);
    app.config_draft.as_mut().expect("draft").base_url =
        InputState::new("https://api.z.ai/api/anthropic");
    app.config_draft.as_mut().expect("draft").api_key = InputState::new("new-key");
    commit_config_field(&mut app, ConfigRow::ApiKey);

    assert!(
        !app.key_rejected_names().contains(&ProfileName::from("api")),
        "a re-keyed account drops out of the live broken set the same frame"
    );

    // Q2: the refresh leg republished the fetch entry with the NEW credential
    // fingerprint same-frame — not merely that verdict authority now ignores the
    // stale list. Read config first (dropped before the token store, per lock
    // order), then the re-collected entry.
    {
        let new_fp = {
            let cfg = app.config();
            let p = cfg
                .find(&ProfileName::from("api"))
                .expect("profile present");
            crate::usage::profile_credential_fingerprint(p)
                .expect("the re-keyed profile is credentialed")
        };
        let list_fp = app
            .third_party_tokens
            .lock()
            .expect("tokens store unpoisoned")
            .iter()
            .find(|e| e.name.as_str() == "api")
            .expect("the re-keyed entry is re-collected same-frame")
            .credential_fingerprint();
        assert_ne!(
            list_fp, old_fp,
            "the re-keyed entry fingerprint differs from the old one"
        );
        assert_eq!(
            list_fp, new_fp,
            "third_party_tokens carries the NEW credential fingerprint immediately"
        );
    }
}

/// Q1 (TUI repair direction): the `on_tick` drain drops a queued switch-away
/// whose record carries a key-rejection cause when the active is re-keyed before
/// dispatch — the active stays and the stale decision is not re-queued.
#[test]
fn on_tick_drops_a_repaired_key_rejected_switch_away() {
    use crate::profile::{AppConfig, AppState, Profile, ProfileName, save_profile};
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["tp", "oauth"]);
    let tp = Profile::new(
        "tp".to_string(),
        Some("https://example.com".to_string()),
        Some("old-key".to_string()),
    );
    save_profile(&tp).expect("save tp");
    let oauth = stored_oauth_profile("oauth", far_future());
    let state = AppState {
        active_profile: Some(ProfileName::from("tp")),
        profiles: vec![ProfileName::from("tp"), ProfileName::from("oauth")],
        fallback_chain: vec![ProfileName::from("tp"), ProfileName::from("oauth")],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let mut app = App::new(AppConfig {
        state,
        profiles: vec![tp, oauth],
    });
    app.bootstrap_started = true;
    let old_fp = crate::usage::profile_credential_fingerprint(
        app.config()
            .find(&ProfileName::from("tp"))
            .expect("tp present"),
    )
    .expect("credentialed");
    app.pending_switch
        .lock()
        .expect("pending")
        .insert(crate::usage::PendingSwitchTarget {
            target: "oauth".to_string(),
            key_rejected_cause: Some(("tp".to_string(), old_fp)),
        });

    // Re-key tp before the drain dispatches.
    {
        let mut cfg = app.config();
        cfg.find_mut(&ProfileName::from("tp"))
            .expect("tp present")
            .api_key = Some("new-key".to_string());
    }

    // First tick drains the queued decision (and drops it); the second would
    // complete a switch had one been dispatched.
    super::on_tick(&mut app);
    super::on_tick(&mut app);

    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some("tp"),
        "the re-keyed active stays put"
    );
    assert!(
        app.pending_switch.lock().expect("pending").is_empty(),
        "the stale switch-away is dropped, not re-queued"
    );
}

/// Q1 (TUI ordinary direction): a cause-absent ordinary exhaustion decision
/// still switches even beside a stale raw broken mark (the leftover of a
/// same-name re-key).
#[test]
fn on_tick_still_switches_an_ordinary_decision_beside_a_stale_mark() {
    use crate::profile::{AppConfig, AppState, Profile, ProfileName, save_profile};
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["tp", "oauth"]);
    let tp = Profile::new(
        "tp".to_string(),
        Some("https://example.com".to_string()),
        Some("old-key".to_string()),
    );
    save_profile(&tp).expect("save tp");
    let oauth = stored_oauth_profile("oauth", far_future());
    let state = AppState {
        active_profile: Some(ProfileName::from("tp")),
        profiles: vec![ProfileName::from("tp"), ProfileName::from("oauth")],
        fallback_chain: vec![ProfileName::from("tp"), ProfileName::from("oauth")],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let mut app = App::new(AppConfig {
        state,
        profiles: vec![tp, oauth],
    });
    app.bootstrap_started = true;

    // A stale mark: recorded under a fingerprint that no longer matches tp's
    // current credential. The ordinary decision carries no cause.
    let stale_fp = crate::usage::profile_credential_fingerprint(&Profile::new(
        "tp".to_string(),
        Some("https://example.com".to_string()),
        Some("other-key".to_string()),
    ))
    .expect("credentialed");
    app.third_party_broken
        .lock()
        .expect("broken")
        .insert("tp".to_string(), stale_fp);
    app.pending_switch
        .lock()
        .expect("pending")
        .insert(crate::usage::PendingSwitchTarget {
            target: "oauth".to_string(),
            key_rejected_cause: None,
        });

    // First tick dispatches (posts the gate); the second completes it.
    super::on_tick(&mut app);
    super::on_tick(&mut app);

    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some("oauth"),
        "the ordinary exhaustion switch still executes beside the stale mark"
    );
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
            ..crate::profile::OAuthToken::default_extra()
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

/// The console arm of the login-row resolver, as the `Option` the old
/// `console_login_target` helper returned before it folded into
/// `login_row_flow`.
fn console_login_target(
    app: &App,
    name: &crate::profile::ProfileName,
) -> Option<(crate::profile::ConsoleSite, &'static str)> {
    match super::login_row_flow(app, Some(name)) {
        super::LoginRowFlow::Console { site, region } => Some((site, region)),
        _ => None,
    }
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

// ── F2: durable key-rejection seeding ────────────────────────────────────────

/// F2: a fingerprint-bound key rejection survives a TUI restart. The durable
/// `third_party_auth.json` verdict (written by the fetching process, read by
/// every no-live-set surface) seeds the in-memory key-rejected set at
/// construction, so the chain/overview/day-list renders see the rejection
/// before any fetch recreates it.
#[test]
fn the_durable_auth_verdict_seeds_the_key_rejected_set_at_construction() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::providers::Provider;
    use crate::tui::app::App;

    let _home = crate::testutil::HomeSandbox::new();

    let mut zai = crate::testutil::blank_profile(&ProfileName::from("zai"));
    zai.provider = Some(Provider::Zai);
    zai.api_key = Some("k".to_string());
    zai.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    crate::profile::save_app_state(&AppState {
        profiles: vec![zai.name.clone()],
        ..AppState::default()
    })
    .expect("persist app state");
    crate::profile::save_profile(&zai).expect("save the profile");

    let fingerprint = crate::usage::profile_credential_fingerprint(&zai)
        .expect("the credential has a fingerprint");
    crate::profile_cache::write_auth_expired(&ProfileName::from("zai"), fingerprint);
    assert!(
        crate::profile_cache::auth_expired_matches(&ProfileName::from("zai"), fingerprint),
        "fixture control: the durable record landed"
    );

    let app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("zai")],
            ..AppState::default()
        },
        profiles: vec![zai],
    });
    assert!(
        app.key_rejected_names().contains(&ProfileName::from("zai")),
        "the durable verdict must survive a restart with no fetch behind it"
    );
}

/// F2: the same seed lands on a config reload — a stood-down TUI picks up a
/// rejection another process recorded after a config write.
#[test]
fn the_durable_auth_verdict_seeds_the_key_rejected_set_on_reload() {
    use crate::profile::{AppState, ProfileName};
    use crate::providers::Provider;
    use crate::testutil::set_mtime;
    use std::time::{Duration, UNIX_EPOCH};

    let home = crate::testutil::HomeSandbox::new();

    let mut zai = crate::testutil::blank_profile(&ProfileName::from("zai"));
    zai.provider = Some(Provider::Zai);
    zai.api_key = Some("k".to_string());
    zai.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    crate::profile::save_app_state(&AppState {
        profiles: vec![zai.name.clone()],
        ..AppState::default()
    })
    .expect("persist app state");
    crate::profile::save_profile(&zai).expect("persist profile config");

    let mut app = app_with(vec![zai.clone()]);

    // The record lands AFTER construction, so the reload leg alone can seed it.
    let fingerprint = crate::usage::profile_credential_fingerprint(&zai)
        .expect("the credential has a fingerprint");
    crate::profile_cache::write_auth_expired(&ProfileName::from("zai"), fingerprint);

    let config_toml = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("zai")
        .join("config.toml");
    set_mtime(&config_toml, UNIX_EPOCH + Duration::from_secs(1_000_000));

    let reloaded = app.reload_if_state_changed();
    assert!(
        reloaded,
        "the reload branch must run so the seed is exercised"
    );
    assert!(
        app.key_rejected_names().contains(&ProfileName::from("zai")),
        "a reload seeds the rejection another process recorded"
    );
}

// ── F3: endpoint/key edits clear stale status chrome ─────────────────────────

/// F3: a successful endpoint/API-key edit clears the edited profile's
/// name-keyed status + streak chrome, so the next frame shows no stale
/// `[ key rejected ]` pill or old-provider throttler attribution.
#[test]
fn commit_endpoint_clears_the_edited_profiles_stale_status_chrome() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::providers::Provider;
    use crate::tui::app::App;
    use crate::usage::FetchStatus;

    let _home = crate::testutil::HomeSandbox::new();

    let mut zai = crate::testutil::blank_profile(&ProfileName::from("zai"));
    zai.provider = Some(Provider::Zai);
    zai.api_key = Some("k".to_string());
    zai.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    crate::profile::save_profile(&zai).expect("save the profile");

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("zai")],
            ..AppState::default()
        },
        profiles: vec![zai],
    });
    app.profile_cursor = 0;
    // Stale chrome the repair must clear: the dead-key verdict, a deep 429
    // streak, and an OAuth-side status entry (the hybrid's other leg).
    app.third_party_status
        .lock()
        .unwrap()
        .insert("zai".to_string(), FetchStatus::AuthExpired);
    app.third_party_streaks
        .lock()
        .unwrap()
        .insert("zai".to_string(), 9);
    // Precondition control: the merge the next frame reads shows the stale
    // key-rejected verdict before the edit.
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("zai"))
            .unwrap()
            .fetch_status,
        Some(FetchStatus::AuthExpired),
        "precondition: the stale verdict is what the frame reads"
    );
    app.usage_status
        .lock()
        .unwrap()
        .insert("zai".to_string(), FetchStatus::Fresh);

    super::enter_config_detail(&mut app);
    if let Some(d) = app.config_draft.as_mut() {
        d.active = Some(super::ConfigRow::ApiKey);
        d.api_key = super::InputState::new("k2");
    }
    super::commit_endpoint(&mut app);

    {
        let m = app.third_party_status.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the dead-key status goes with the repaired credential"
        );
    }
    {
        let m = app.usage_status.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the OAuth-side status goes with the repaired credential"
        );
    }
    {
        let m = app.third_party_streaks.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the old provider's streak cannot accuse the new credential"
        );
    }

    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("zai"))
            .unwrap()
            .fetch_status,
        None,
        "the next frame shows no stale chrome after a repaired key"
    );
}

/// R2-F2: a stood-down TUI's seeded key-rejection heals when the fetcher
/// (another process) clears the durable record under the SAME credential — no
/// config change, so the reload seed never sees it. The once-per-second
/// verdict sync notices the record's disappearance and drops the mirror entry.
#[test]
fn the_seeded_key_rejection_heals_when_another_process_clears_the_record() {
    use crate::profile::{AppState, ProfileName};
    use crate::providers::Provider;

    let _home = crate::testutil::HomeSandbox::new();

    let mut zai = crate::testutil::blank_profile(&ProfileName::from("zai"));
    zai.provider = Some(Provider::Zai);
    zai.api_key = Some("k".to_string());
    zai.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    crate::profile::save_app_state(&AppState {
        profiles: vec![zai.name.clone()],
        ..AppState::default()
    })
    .expect("persist app state");
    crate::profile::save_profile(&zai).expect("save the profile");

    let fingerprint = crate::usage::profile_credential_fingerprint(&zai)
        .expect("the credential has a fingerprint");
    crate::profile_cache::write_auth_expired(&ProfileName::from("zai"), fingerprint);

    let mut app = app_with(vec![zai]);
    assert!(
        app.key_rejected_names().contains(&ProfileName::from("zai")),
        "precondition: the construction seed marks the rejected member"
    );

    // The fetcher healed the credential: the durable record is gone while the
    // credential (and its fingerprint) is unchanged.
    crate::profile_cache::clear_auth_expired(&ProfileName::from("zai"));
    assert!(
        !crate::profile_cache::auth_expired_matches(&ProfileName::from("zai"), fingerprint),
        "fixture control: the record is really gone"
    );

    super::on_tick(&mut app);
    assert!(
        !app.key_rejected_names().contains(&ProfileName::from("zai")),
        "the healed member must leave the key-rejected set without a config change"
    );
}

/// R2-F2: a rejection recorded by another process AFTER construction seeds
/// the mirror on the cadence — the fixture writes the record file directly,
/// the way the fetcher's durable writer does.
#[test]
fn a_record_written_by_another_process_seeds_the_key_rejected_set_on_the_cadence() {
    use crate::profile::{AppState, ProfileName};
    use crate::providers::Provider;

    let _home = crate::testutil::HomeSandbox::new();

    let mut zai = crate::testutil::blank_profile(&ProfileName::from("zai"));
    zai.provider = Some(Provider::Zai);
    zai.api_key = Some("k".to_string());
    zai.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    crate::profile::save_app_state(&AppState {
        profiles: vec![zai.name.clone()],
        ..AppState::default()
    })
    .expect("persist app state");
    crate::profile::save_profile(&zai).expect("save the profile");

    let fingerprint = crate::usage::profile_credential_fingerprint(&zai)
        .expect("the credential has a fingerprint");
    let mut app = app_with(vec![zai]);
    assert!(
        !app.key_rejected_names().contains(&ProfileName::from("zai")),
        "precondition: nothing rejected yet"
    );

    // The fetcher recorded a rejection under this credential; write the
    // record file directly, as the fetcher's durable writer would.
    let path = crate::profile_cache::profile_cache_path(
        &ProfileName::from("zai"),
        crate::profile_cache::THIRD_PARTY_AUTH_FILE,
    )
    .expect("the record path resolves");
    std::fs::write(&path, format!("{{\"credential\":{fingerprint}}}")).expect("write the record");

    super::on_tick(&mut app);
    assert!(
        app.key_rejected_names().contains(&ProfileName::from("zai")),
        "a rejection recorded by another process must appear without a config change"
    );
}

/// R2-F3: a captured-login overwrite retires the old provider's verdicts the
/// same way a credential edit does — one shared clear helper, not a second
/// spelling. The pre-overwrite frame reads the stale `[ key rejected ]`
/// verdict; the post-overwrite frame reads nothing.
#[test]
fn a_capture_overwrite_clears_the_stale_status_chrome() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::providers::Provider;
    use crate::tui::app::App;
    use crate::usage::FetchStatus;

    let _home = crate::testutil::HomeSandbox::new();

    let mut zai = crate::testutil::blank_profile(&ProfileName::from("zai"));
    zai.provider = Some(Provider::Zai);
    zai.api_key = Some("k".to_string());
    zai.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    crate::profile::save_profile(&zai).expect("save the profile");

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("zai")],
            ..AppState::default()
        },
        profiles: vec![zai],
    });
    app.third_party_status
        .lock()
        .unwrap()
        .insert("zai".to_string(), FetchStatus::AuthExpired);
    app.third_party_streaks
        .lock()
        .unwrap()
        .insert("zai".to_string(), 9);
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("zai"))
            .unwrap()
            .fetch_status,
        Some(FetchStatus::AuthExpired),
        "precondition: the stale verdict is what the frame reads"
    );
    app.usage_status
        .lock()
        .unwrap()
        .insert("zai".to_string(), FetchStatus::Fresh);

    super::run_confirm_action(
        &mut app,
        super::ConfirmAction::CaptureOverwrite(
            Box::new(crate::actions::CaptureSnapshot {
                credentials: None,
                base_url: None,
                api_key: None,
                anchor: crate::actions::AnchorAction::Unproven,
            }),
            "zai".to_string(),
            false,
        ),
    );

    {
        let m = app.third_party_status.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the dead-key status goes with the overwritten credential"
        );
    }
    {
        let m = app.third_party_streaks.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the old provider's streak cannot accuse the new credential"
        );
    }
    {
        let m = app.usage_status.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the OAuth-side status goes with the overwritten credential"
        );
    }
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("zai"))
            .unwrap()
            .fetch_status,
        None,
        "the next frame shows no stale chrome after an overwrite"
    );
}

/// R3-F1: the sync's insert branch runs the seed's own shape gate. A
/// `ConsoleExpired` outcome writes the durable record under a still-matching
/// fingerprint, but a lapsed Alibaba console session is usage-only and must
/// never render as key-rejected. The construction seed (which shares the
/// predicate) is the control: it already leaves the name alone.
#[test]
fn a_console_lapse_record_never_seeds_the_key_rejected_set() {
    use crate::profile::{AppState, ConsoleCredential, ConsoleSite, ProfileName};
    use crate::providers::Provider;

    let _home = crate::testutil::HomeSandbox::new();

    let mut qwen = crate::testutil::blank_profile(&ProfileName::from("qwen"));
    qwen.base_url =
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string());
    qwen.provider = Provider::from_base_url(
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
    );
    qwen.api_key = Some("k".to_string());
    qwen.console = Some(ConsoleCredential {
        token: "dead".to_string(),
        site: ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    });
    crate::profile::save_app_state(&AppState {
        profiles: vec![qwen.name.clone()],
        ..AppState::default()
    })
    .expect("persist app state");
    crate::profile::save_profile(&qwen).expect("save the profile");

    // The record the leg writes for `ConsoleExpired` — bound to the same
    // fingerprint the profile still carries.
    let fingerprint = crate::usage::profile_credential_fingerprint(&qwen)
        .expect("the credential has a fingerprint");
    crate::profile_cache::write_auth_expired(&ProfileName::from("qwen"), fingerprint);
    assert!(
        crate::profile_cache::auth_expired_matches(&ProfileName::from("qwen"), fingerprint),
        "fixture control: the console-lapse record landed"
    );

    let mut app = app_with(vec![qwen]);
    assert!(
        !app.key_rejected_names()
            .contains(&ProfileName::from("qwen")),
        "control: the construction seed's shape gate leaves the lapsed console alone"
    );

    super::on_tick(&mut app);
    assert!(
        !app.key_rejected_names()
            .contains(&ProfileName::from("qwen")),
        "the cadence must not seed a console lapse as key-rejected"
    );
}

/// R3-F1: the same shape gate drops a mirror entry that already exists for a
/// console-lapsing profile — the insert branch requires the predicate, so a
/// name the predicate refuses must leave on the cadence.
#[test]
fn a_console_lapse_drops_a_pre_existing_mirror_entry_on_the_cadence() {
    use crate::profile::{AppState, ConsoleCredential, ConsoleSite, ProfileName};
    use crate::providers::Provider;

    let _home = crate::testutil::HomeSandbox::new();

    let mut qwen = crate::testutil::blank_profile(&ProfileName::from("qwen"));
    qwen.base_url =
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string());
    qwen.provider = Provider::from_base_url(
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
    );
    qwen.api_key = Some("k".to_string());
    qwen.console = Some(ConsoleCredential {
        token: "dead".to_string(),
        site: ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    });
    crate::profile::save_app_state(&AppState {
        profiles: vec![qwen.name.clone()],
        ..AppState::default()
    })
    .expect("persist app state");
    crate::profile::save_profile(&qwen).expect("save the profile");

    let fingerprint = crate::usage::profile_credential_fingerprint(&qwen)
        .expect("the credential has a fingerprint");
    crate::profile_cache::write_auth_expired(&ProfileName::from("qwen"), fingerprint);

    let mut app = app_with(vec![qwen]);
    app.third_party_broken
        .lock()
        .unwrap()
        .insert("qwen".to_string(), fingerprint);
    assert!(
        app.key_rejected_names()
            .contains(&ProfileName::from("qwen")),
        "precondition: the planted mirror entry is visible through the intersection"
    );

    super::on_tick(&mut app);
    assert!(
        !app.key_rejected_names()
            .contains(&ProfileName::from("qwen")),
        "a name the shape gate refuses must drop from the mirror on the cadence"
    );
}

/// R3-F2: a blanked third-party api key retires the old provider's verdicts —
/// no leg ever fetches a keyless profile, so a stale `[ key rejected ]` would
/// render forever without the shared clear.
#[test]
fn a_blanked_key_clears_the_stale_status_chrome() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::providers::Provider;
    use crate::tui::app::App;
    use crate::usage::FetchStatus;

    let _home = crate::testutil::HomeSandbox::new();

    let mut zai = crate::testutil::blank_profile(&ProfileName::from("zai"));
    zai.provider = Some(Provider::Zai);
    zai.api_key = Some("k".to_string());
    zai.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    crate::profile::save_profile(&zai).expect("save the profile");

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("zai")],
            ..AppState::default()
        },
        profiles: vec![zai],
    });
    app.third_party_status
        .lock()
        .unwrap()
        .insert("zai".to_string(), FetchStatus::AuthExpired);
    app.third_party_streaks
        .lock()
        .unwrap()
        .insert("zai".to_string(), 9);
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("zai"))
            .unwrap()
            .fetch_status,
        Some(FetchStatus::AuthExpired),
        "precondition: the stale verdict is what the frame reads"
    );
    app.usage_status
        .lock()
        .unwrap()
        .insert("zai".to_string(), FetchStatus::Fresh);

    super::run_confirm_action(
        &mut app,
        super::ConfirmAction::BlankCredentials("zai".to_string()),
    );

    {
        let m = app.third_party_status.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the dead-key status goes with the blanked credential"
        );
    }
    {
        let m = app.third_party_streaks.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the old provider's streak cannot outlive the blanked key"
        );
    }
    {
        let m = app.usage_status.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the OAuth-side status goes with the blanked credential"
        );
    }
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("zai"))
            .unwrap()
            .fetch_status,
        None,
        "the next frame shows no stale chrome after a blanked key"
    );
}

/// R6: the endpoint re-key retires the stale `[ key rejected ]` verdict in
/// the config handle's profile copy SAME-FRAME. The shared clear drops the
/// three name-keyed stores, but the frame reads `Profile.fetch_status` from
/// the config handle, and only the next 80 ms tick's `apply_usage` copies
/// the cleared stores back — so the frame(s) drawn immediately after the
/// commit flashed the retired pill. The clear now retires the handle copy
/// too (the same assignment `apply_usage` derives from the cleared stores),
/// so the frame right after the commit reads none: no `apply_usage` between
/// the edit and the assertion.
#[test]
fn a_rekey_retires_the_handle_fetch_status_before_the_next_tick() {
    use super::{ConfigRow, InputState, commit_config_field, enter_config_detail};
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::providers::Provider;
    use crate::tui::app::App;
    use crate::usage::FetchStatus;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["api"]);

    let mut api = crate::testutil::blank_profile(&ProfileName::from("api"));
    api.provider = Some(Provider::Zai);
    api.api_key = Some("old-key".to_string());
    api.base_url = Some("https://api.z.ai/api/anthropic".to_string());

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("api")],
            ..AppState::default()
        },
        profiles: vec![api],
    });

    // The rejected-key verdict lives in the third-party status store and the
    // tick copies it into the handle — the populated state the frame renders
    // the `[ key rejected ]` pill from.
    app.third_party_status
        .lock()
        .unwrap()
        .insert("api".to_string(), FetchStatus::AuthExpired);
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("api"))
            .unwrap()
            .fetch_status,
        Some(FetchStatus::AuthExpired),
        "precondition: the stale verdict is what the frame reads"
    );

    // Re-key through the real producer (the Setup endpoint editor's ⏎).
    app.profile_cursor = 0;
    enter_config_detail(&mut app);
    app.config_draft.as_mut().expect("draft").base_url = InputState::new("https://new.example.com");
    commit_config_field(&mut app, ConfigRow::BaseUrl);
    app.config_draft.as_mut().expect("draft").api_key = InputState::new("new-key");
    commit_config_field(&mut app, ConfigRow::ApiKey);

    {
        let m = app.third_party_status.lock().unwrap();
        assert!(
            !m.contains_key("api"),
            "the dead-key status store entry goes with the re-key"
        );
    }
    assert_eq!(
        app.config()
            .find(&ProfileName::from("api"))
            .unwrap()
            .fetch_status,
        None,
        "the frame right after the re-key reads no stale [ key rejected ] pill"
    );
}

/// R8: a poison-skipped status-store clear cannot strand the stale pill on
/// the handle. The store clears are poison-tolerant — a store lock poisoned
/// at the clear instant keeps its entry — but that entry is dead weight:
/// `apply_usage` reads these stores with `.lock().ok()` and skips a poisoned
/// one, so nothing re-copies it. The unconditional handle write retires the
/// pill the instant the credential is repaired, and the next tick's merge
/// cannot resurrect it from the surviving entry (the healthy OAuth store's
/// cleared entry misses the `contains_key` arm and the poisoned third-party
/// fallback reads as `None`).
#[test]
fn a_poison_skipped_clear_still_retires_the_handle_verdict() {
    use super::{ConfigRow, InputState, commit_config_field, enter_config_detail};
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::providers::Provider;
    use crate::tui::app::App;
    use crate::usage::FetchStatus;

    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["api"]);

    let mut api = crate::testutil::blank_profile(&ProfileName::from("api"));
    api.provider = Some(Provider::Zai);
    api.api_key = Some("old-key".to_string());
    api.base_url = Some("https://api.z.ai/api/anthropic".to_string());

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("api")],
            ..AppState::default()
        },
        profiles: vec![api],
    });

    // The rejected-key verdict lives in both status stores; the third-party
    // one (rank 280) is the clear we poison, the OAuth-side one (350) the
    // clear that still succeeds — the skipped clear must not be able to
    // strand the stale pill on the handle.
    app.third_party_status
        .lock()
        .unwrap()
        .insert("api".to_string(), FetchStatus::AuthExpired);
    app.usage_status
        .lock()
        .unwrap()
        .insert("api".to_string(), FetchStatus::AuthExpired);
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("api"))
            .unwrap()
            .fetch_status,
        Some(FetchStatus::AuthExpired),
        "precondition: the stale verdict is what the frame reads"
    );

    // Poison it the only way a mutex gets poisoned: panic while holding it
    // (the lockorder.rs / oauth.rs fixtures' shape). The store stays poisoned
    // for the rest of the test, so the clear below hits it exactly as a
    // panic-at-the-clear-instant would.
    let poisoner = Arc::clone(&app.third_party_status);
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = poisoner.lock().expect("lock the store");
        panic!("deliberate: poison the third_party_status store");
    }));
    assert!(poisoned.is_err(), "precondition: the closure panicked");

    // Re-key through the real producer (the Setup endpoint editor's ⏎) —
    // the round-6 flash test's exact sequence.
    app.profile_cursor = 0;
    enter_config_detail(&mut app);
    app.config_draft.as_mut().expect("draft").base_url = InputState::new("https://new.example.com");
    commit_config_field(&mut app, ConfigRow::BaseUrl);
    app.config_draft.as_mut().expect("draft").api_key = InputState::new("new-key");
    commit_config_field(&mut app, ConfigRow::ApiKey);

    {
        let m = app.usage_status.lock().unwrap();
        assert!(
            !m.contains_key("api"),
            "the OAuth-side status entry goes with the re-key"
        );
    }
    {
        let m = app.third_party_streaks.lock().unwrap();
        assert!(
            !m.contains_key("api"),
            "the old provider's streak cannot outlive the re-key"
        );
    }
    // The poison-skipped store still holds its entry (`lock` recovers it
    // through `into_inner`, the repo's documented idiom) — the dead weight
    // the next tick's merge cannot reach.
    {
        let m = app
            .third_party_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            m.contains_key("api"),
            "precondition: the poisoned store's entry survives the skipped clear"
        );
    }
    assert_eq!(
        app.config()
            .find(&ProfileName::from("api"))
            .unwrap()
            .fetch_status,
        None,
        "the handle retires the pill even when a poisoned store keeps the stale entry"
    );
    // The next tick's merge cannot resurrect the retired pill: the healthy
    // OAuth store's cleared entry misses the `contains_key` arm and the
    // poisoned third-party fallback reads as `None`.
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("api"))
            .unwrap()
            .fetch_status,
        None,
        "the next tick does not resurrect the pill from the surviving entry"
    );
}

/// R3-F2: an Alibaba console re-login retires the stale `console login
/// expired` verdict — the shared clear, not a wait for the queued fetch.
#[test]
fn a_console_relogin_clears_the_stale_verdict() {
    use crate::profile::{AppConfig, AppState, ConsoleCredential, ConsoleSite, ProfileName};
    use crate::providers::Provider;
    use crate::tui::app::App;
    use crate::usage::FetchStatus;

    let _home = crate::testutil::HomeSandbox::new();

    let mut qwen = crate::testutil::blank_profile(&ProfileName::from("qwen"));
    qwen.base_url =
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string());
    qwen.provider = Provider::from_base_url(
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
    );
    qwen.api_key = Some("k".to_string());
    qwen.console = Some(ConsoleCredential {
        token: "dead".to_string(),
        site: ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    });
    crate::profile::save_profile(&qwen).expect("save the profile");

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("qwen")],
            ..AppState::default()
        },
        profiles: vec![qwen],
    });
    app.third_party_status
        .lock()
        .unwrap()
        .insert("qwen".to_string(), FetchStatus::AuthExpired);
    app.third_party_streaks
        .lock()
        .unwrap()
        .insert("qwen".to_string(), 9);
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("qwen"))
            .unwrap()
            .fetch_status,
        Some(FetchStatus::AuthExpired),
        "precondition: the stale console verdict is what the frame reads"
    );

    super::apply_console_login(
        &mut app,
        super::LoginSession {
            name: "qwen".to_string(),
            is_new: false,
            generation: 0,
            url: None,
            stage: super::LoginStage::WaitingBrowser,
            method: super::LoginMethod::Browser,
            paste: None,
            paste_field: None,
        },
        crate::alibaba_login::ConsoleLoginOutcome {
            console: ConsoleCredential {
                token: "fresh".to_string(),
                site: ConsoleSite::International,
                region: "ap-southeast-1".to_string(),
            },
        },
    );

    {
        let m = app.third_party_status.lock().unwrap();
        assert!(
            !m.contains_key("qwen"),
            "the stale console verdict goes with the fresh session"
        );
    }
    {
        let m = app.third_party_streaks.lock().unwrap();
        assert!(
            !m.contains_key("qwen"),
            "the old session's streak cannot outlive the re-login"
        );
    }
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("qwen"))
            .unwrap()
            .fetch_status,
        None,
        "the next frame shows no stale verdict after a console re-login"
    );
}

/// R3-F2: applying a preset repoints the base_url — the old provider's streak
/// must go with the shared clear, or the throttler hint can accuse the new
/// endpoint with the old provider's evidence.
#[test]
fn a_preset_apply_clears_the_old_providers_streak() {
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::providers::Provider;
    use crate::tui::app::App;
    use crate::usage::FetchStatus;

    let _home = crate::testutil::HomeSandbox::new();

    let mut zai = crate::testutil::blank_profile(&ProfileName::from("zai"));
    zai.provider = Some(Provider::Zai);
    zai.api_key = Some("k".to_string());
    zai.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    crate::profile::save_profile(&zai).expect("save the profile");

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("zai")],
            ..AppState::default()
        },
        profiles: vec![zai],
    });
    app.third_party_status
        .lock()
        .unwrap()
        .insert("zai".to_string(), FetchStatus::RateLimited);
    app.third_party_streaks
        .lock()
        .unwrap()
        .insert("zai".to_string(), crate::usage::ACTIVE_CAP_MAX_STREAK + 1);
    assert!(
        app.third_party_streaks.lock().unwrap().contains_key("zai"),
        "precondition: the old provider's deep streak is on the books"
    );

    super::apply_preset_to(&mut app, "zai", "DeepSeek");

    {
        let m = app.third_party_streaks.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the repointed endpoint must not inherit the old provider's streak"
        );
    }
    {
        let m = app.third_party_status.lock().unwrap();
        assert!(
            !m.contains_key("zai"),
            "the old provider's fetch status goes with the repoint"
        );
    }
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("zai"))
            .unwrap()
            .fetch_status,
        None,
        "the next frame shows no stale chrome after a preset repoint"
    );
}

/// R4: the divergence resolver's Overwrite choice snapshots the LIVE login
/// into the active profile's stored credential — a credential replacement, so
/// the OAuth leg's stale verdict must retire with it, not survive until the
/// next fetch. The choice is driven through the modal's real Enter router
/// (`handle_divergence_key`), the live UI path, with the stale verdict
/// already on the books.
#[test]
fn a_divergence_overwrite_clears_the_stale_status_chrome() {
    use super::{Modal, handle_key};
    use crate::profile::{AppConfig, AppState, Profile, ProfileName, save_app_state, save_profile};
    use crate::testutil::key;
    use crate::tui::app::App;
    use crate::usage::FetchStatus;
    use ratatui::crossterm::event::KeyCode;

    let _home = crate::testutil::HomeSandbox::new();

    // Stored `rt-work` vs the live file's `rt-live`: a diverged active
    // profile, the state the resolver exists for.
    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_app_state(&AppState {
        profiles: vec![ProfileName::from("work")],
        ..AppState::default()
    })
    .expect("persist app state");
    save_profile(&work).expect("save the profile");
    write_live_creds(&creds_ra("rt-live", "at-1"));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some(ProfileName::from("work")),
            profiles: vec![ProfileName::from("work")],
            ..AppState::default()
        },
        profiles: vec![work],
    });

    // The OAuth leg's stale verdict on the active profile, applied through
    // the store — the pill the frame reads before the overwrite.
    app.usage_status
        .lock()
        .unwrap()
        .insert("work".to_string(), FetchStatus::AuthExpired);
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("work"))
            .unwrap()
            .fetch_status,
        Some(FetchStatus::AuthExpired),
        "precondition: the stale verdict is what the frame reads"
    );

    // The live UI path: the poll flags the banner, `d` opens the resolver,
    // Enter fires the Overwrite choice (cursor 0, no sibling).
    force_poll(&mut app);
    handle_key(&mut app, key(KeyCode::Char('d')));
    assert!(
        matches!(app.modals.last(), Some(Modal::Divergence(_))),
        "precondition: the resolver opened on the diverged profile"
    );
    handle_key(&mut app, key(KeyCode::Enter));

    assert_eq!(
        app.config()
            .find(&ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
        Some("rt-live"),
        "the overwrite replaces the stored credential with the live one"
    );
    {
        let m = app.usage_status.lock().unwrap();
        assert!(
            !m.contains_key("work"),
            "the OAuth-side verdict goes with the overwritten credential"
        );
    }
    app.apply_usage();
    assert_eq!(
        app.config()
            .find(&ProfileName::from("work"))
            .unwrap()
            .fetch_status,
        None,
        "the next frame shows no stale chrome after a divergence overwrite"
    );
}

/// R5: the divergence resolver's Overwrite choice stores a fresh live OAuth
/// login — the documented recovery for a revoked chain — so a standing
/// name-bound `auth_broken` quarantine must lift with it (AUTH-1), like on
/// every sibling fresh-login path. Without the lift a repaired member keeps
/// rendering the `[ auth broken ]` chrome and stays out of chain walks
/// forever: the poll skips the dead refresh, so nothing else clears it.
/// The choice is driven through the modal's real Enter router
/// (`handle_divergence_key`), the live UI path.
#[test]
fn a_divergence_overwrite_lifts_the_auth_broken_quarantine() {
    use super::{Modal, handle_key};
    use crate::profile::{AppConfig, AppState, Profile, ProfileName, save_app_state, save_profile};
    use crate::testutil::key;
    use crate::tui::app::App;
    use ratatui::crossterm::event::KeyCode;

    let _home = crate::testutil::HomeSandbox::new();

    // Stored `rt-work` vs the live file's `rt-live`: a diverged active
    // profile carrying a persisted quarantine row — the state the finding
    // names.
    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_app_state(&AppState {
        profiles: vec![ProfileName::from("work")],
        auth_broken: vec![ProfileName::from("work")],
        ..AppState::default()
    })
    .expect("persist app state");
    save_profile(&work).expect("save the profile");
    write_live_creds(&creds_ra("rt-live", "at-1"));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some(ProfileName::from("work")),
            profiles: vec![ProfileName::from("work")],
            auth_broken: vec![ProfileName::from("work")],
            ..AppState::default()
        },
        profiles: vec![work],
    });

    {
        let persisted = crate::profile::load_config().expect("reload").state;
        assert!(
            persisted.auth_broken.iter().any(|n| n.as_str() == "work"),
            "precondition: the quarantine row is persisted"
        );
    }
    assert!(
        app.config().is_auth_broken(&ProfileName::from("work")),
        "precondition: the frame renders the auth-broken chrome"
    );

    // The live UI path: the poll flags the banner, `d` opens the resolver,
    // Enter fires the Overwrite choice (cursor 0, no sibling).
    force_poll(&mut app);
    handle_key(&mut app, key(KeyCode::Char('d')));
    assert!(
        matches!(app.modals.last(), Some(Modal::Divergence(_))),
        "precondition: the resolver opened on the diverged profile"
    );
    handle_key(&mut app, key(KeyCode::Enter));

    assert_eq!(
        app.config()
            .find(&ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
        Some("rt-live"),
        "the overwrite replaces the stored credential with the live one"
    );
    assert!(
        !app.config().is_auth_broken(&ProfileName::from("work")),
        "the next frame renders no [ auth broken ] chrome after an overwrite"
    );
    let persisted = crate::profile::load_config().expect("reload").state;
    assert!(
        !persisted.auth_broken.iter().any(|n| n.as_str() == "work"),
        "the persisted quarantine row goes with the overwritten credential"
    );
}

/// R5: the first-login adopt stores a fresh live OAuth login the same way —
/// the poll adopts it silently on a credential-less active profile (the
/// shell `Profile::new` state `BlankCredentials` stores), so a standing
/// `auth_broken` quarantine must lift with it (AUTH-1), like on every
/// sibling fresh-login path. Driven through the real poll the live caller
/// uses (`poll_credentials_divergence`), no direct call to
/// `adopt_first_login`.
#[test]
fn an_adopted_first_login_lifts_the_auth_broken_quarantine() {
    use crate::profile::{AppConfig, AppState, Profile, ProfileName, save_app_state, save_profile};
    use crate::tui::app::App;

    let _home = crate::testutil::HomeSandbox::new();

    // A credential-less (blanked) active profile: no stored credentials.json,
    // a standing quarantine row, and a fresh live login CC just wrote.
    let work = Profile::new("work".to_string(), None, None);
    save_app_state(&AppState {
        profiles: vec![ProfileName::from("work")],
        auth_broken: vec![ProfileName::from("work")],
        ..AppState::default()
    })
    .expect("persist app state");
    save_profile(&work).expect("save the profile");
    write_live_creds(&creds_ra("rt-live", "at-1"));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some(ProfileName::from("work")),
            profiles: vec![ProfileName::from("work")],
            auth_broken: vec![ProfileName::from("work")],
            ..AppState::default()
        },
        profiles: vec![work],
    });

    {
        let persisted = crate::profile::load_config().expect("reload").state;
        assert!(
            persisted.auth_broken.iter().any(|n| n.as_str() == "work"),
            "precondition: the quarantine row is persisted"
        );
    }
    assert!(
        app.config().is_auth_broken(&ProfileName::from("work")),
        "precondition: the frame renders the auth-broken chrome"
    );

    // The real adopt path: the poll sees the fresh live login on the
    // credential-less active profile and adopts it silently.
    force_poll(&mut app);

    assert_eq!(
        app.config()
            .find(&ProfileName::from("work"))
            .and_then(|p| p.refresh_token()),
        Some("rt-live"),
        "the poll adopts the first login into the stored credential"
    );
    assert!(
        !app.config().is_auth_broken(&ProfileName::from("work")),
        "the next frame renders no [ auth broken ] chrome after an adopt"
    );
    let persisted = crate::profile::load_config().expect("reload").state;
    assert!(
        !persisted.auth_broken.iter().any(|n| n.as_str() == "work"),
        "the persisted quarantine row goes with the adopted login"
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

/// A live-session delete must not dead-end on the guard's refusal toast: it
/// arms a confirm modal instead, leaving the profile untouched until confirmed.
#[test]
fn perform_delete_with_live_session_arms_a_confirm_modal() {
    use super::{ConfirmAction, Modal, perform_delete, run_confirm_action};
    use crate::profile::Profile;
    let home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![Profile::new("busy".to_string(), None, None)]);
    let _pid_guard = crate::testutil::arm_live_session(home.home(), "busy");

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
    let _pid_guard = crate::testutil::arm_live_session(home.home(), "acct");
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
            ("start daemon", Some('s')),
            ("start shunt", None),
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
            ("start daemon", Some('s')),
            ("start shunt", None),
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
            ("new account", Some('n')),
            ("start daemon", Some('s')),
            ("start shunt", Some('t')),
        ]
    );
}

/// Every tab's menu ends on the one daemon verb that applies: `start daemon`
/// with no daemon up, `stop daemon` with one up, fresh or stale, and neither
/// while a start or stop is in flight. It rides the tab-global group.
#[test]
fn every_tab_offers_the_daemon_verb_that_applies() {
    use super::{Tab, build_action_menu};
    use crate::daemon::DaemonHealth;
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
    app.profile_cursor = 0;
    let daemon_verbs = |app: &super::App| -> Vec<&'static str> {
        build_action_menu(app)
            .items
            .iter()
            .map(|i| i.label)
            .filter(|l| l.ends_with(" daemon"))
            .collect()
    };

    for tab in Tab::ALL {
        app.tab = tab;
        for (health, verb) in [
            (DaemonHealth::Absent, "start daemon"),
            (DaemonHealth::Stale, "stop daemon"),
            (DaemonHealth::Fresh, "stop daemon"),
        ] {
            app.daemon_health = health;
            app.daemon_control_busy = false;
            let menu = build_action_menu(&app);
            // The shunt verb, when one applies, sits directly after the daemon
            // verb, so the daemon verb is no longer last.
            let daemon_idx = menu
                .items
                .iter()
                .rposition(|i| i.label.ends_with(" daemon"));
            let shunt_idx = menu.items.iter().position(|i| i.label.ends_with(" shunt"));
            assert_eq!(
                daemon_idx.map(|i| menu.items[i].label),
                Some(verb),
                "{tab:?} with the daemon {health:?}"
            );
            assert_eq!(
                shunt_idx,
                daemon_idx.map(|i| i + 1),
                "{tab:?}: the shunt verb follows the daemon verb"
            );
            assert!(
                menu.scoped_len < menu.items.len(),
                "{tab:?}: the verb is tab-global, never under the account's name"
            );
            assert_eq!(
                daemon_verbs(&app),
                [verb],
                "{tab:?}: only the verb that applies"
            );
            app.daemon_control_busy = true;
            assert!(
                daemon_verbs(&app).is_empty(),
                "{tab:?}: no verb while one is in flight"
            );
        }
    }
}

/// Each start/stop outcome lands as its own toast, frees the menu's verb, and
/// re-probes the header chip at once rather than a throttled second later.
#[test]
fn daemon_control_outcomes_toast_and_rearm_the_verb() {
    use super::{DaemonControlResult as R, ToastKind};
    use crate::daemon::{DaemonHealth, DaemonStop, StartOutcome};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    let cases = [
        (
            R::Start(Ok(StartOutcome::Holding)),
            ToastKind::Success,
            "daemon started",
        ),
        (
            R::Start(Ok(StartOutcome::Exited)),
            ToastKind::Danger,
            "daemon exited at start\nsee ~/.clauth/daemon.log",
        ),
        (
            R::Start(Ok(StartOutcome::NotYet)),
            ToastKind::Warning,
            "daemon not up after 5s\nsee ~/.clauth/daemon.log",
        ),
        (
            R::Start(Err("no such file".to_string())),
            ToastKind::Danger,
            "daemon start failed\nno such file",
        ),
        (
            R::Stop(Ok(DaemonStop::Stopped)),
            ToastKind::Success,
            "daemon stopped",
        ),
        (
            R::Stop(Ok(DaemonStop::NotRunning)),
            ToastKind::Info,
            "no daemon was running",
        ),
        (
            R::Stop(Ok(DaemonStop::Replaced)),
            ToastKind::Warning,
            "daemon stopped\nanother daemon took over",
        ),
        (
            R::Stop(Err("wedged".to_string())),
            ToastKind::Danger,
            "daemon stop failed\nwedged",
        ),
    ];
    for (result, kind, body) in cases {
        app.daemon_control_busy = true;
        app.daemon_health = DaemonHealth::Fresh;
        app.daemon_control_tx.send(result).expect("send");
        super::drain_daemon_control(&mut app);
        let toast = app.toasts.back().expect("a toast");
        assert_eq!((toast.kind, toast.body.as_str()), (kind, body));
        assert!(!app.daemon_control_busy, "{body:?} frees the verb");
        assert_eq!(
            app.daemon_health,
            DaemonHealth::Absent,
            "{body:?} re-probed the chip (no daemon in the sandbox)"
        );
    }
}

/// A daemon started elsewhere while the start confirm sat open meets no second
/// spawn: `start` reads the held singleton, says so, and re-probes the chip.
#[test]
fn the_daemon_start_confirm_spawns_nothing_once_a_daemon_runs() {
    use super::{ActionMenuAction, ConfirmAction, ToastKind, dispatch_action_menu_action};
    use crate::daemon::DaemonHealth;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();

    dispatch_action_menu_action(&mut app, ActionMenuAction::StartDaemon);
    assert!(matches!(
        app.modals.last(),
        Some(super::Modal::Confirm(state)) if matches!(state.on_confirm, ConfirmAction::StartDaemon)
    ));
    app.modals.pop();
    assert_eq!(app.daemon_health, DaemonHealth::Absent);

    let _held = crate::daemon::hold_daemon_lock();
    super::run_confirm_action(&mut app, ConfirmAction::StartDaemon);
    let toasts: Vec<(ToastKind, &str)> = app
        .toasts
        .iter()
        .map(|t| (t.kind, t.body.as_str()))
        .collect();
    assert_eq!(toasts, vec![(ToastKind::Info, "daemon already running")]);
    assert!(
        !app.daemon_control_busy,
        "no start runs, so the verb stays armed"
    );
    assert_eq!(
        app.daemon_health,
        DaemonHealth::Stale,
        "the chip re-probes at once: a held lock with no feed reads stale"
    );
}

/// A lock file `start` cannot read is no "no daemon": it refuses, naming the
/// file, and spawns nothing.
#[test]
fn the_daemon_start_confirm_refuses_an_unreadable_lock() {
    use super::{ConfirmAction, ToastKind};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    let lock = crate::daemon::daemon_lock_path();
    std::fs::create_dir_all(&lock).unwrap();

    super::run_confirm_action(&mut app, ConfirmAction::StartDaemon);
    let [toast] = app.toasts.iter().collect::<Vec<_>>()[..] else {
        panic!("one toast, got {:?}", app.toasts);
    };
    assert_eq!(toast.kind, ToastKind::Danger);
    // Opening a directory for writing: EISDIR on unix, ERROR_ACCESS_DENIED on
    // Windows.
    let cause = std::io::Error::from_raw_os_error(if cfg!(windows) { 5 } else { 21 });
    assert_eq!(
        toast.body,
        format!(
            "daemon start failed\nfailed to open the clauth daemon lock file {}: {cause}",
            lock.display()
        )
    );
    assert!(!app.daemon_control_busy);
}

/// The menu's verbs reach their workers and the tick lands their outcome. A
/// test build refuses to start a daemon, since its binary is the test harness,
/// and says so; the stop runs the real termination, which finds no daemon in
/// the sandbox.
#[test]
fn the_daemon_verbs_reach_their_workers() {
    use super::{ActionMenuAction, ConfirmAction, dispatch_action_menu_action, join_test_workers};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();

    // `start daemon` opens the shared confirm now; its `start` runs the start.
    dispatch_action_menu_action(&mut app, ActionMenuAction::StartDaemon);
    let Some(super::Modal::Confirm(state)) = app.modals.last() else {
        panic!("start daemon must open the confirm, got {:?}", app.modals);
    };
    assert_eq!(state.message, "start daemon?");
    assert!(
        matches!(state.on_confirm, ConfirmAction::StartDaemon),
        "the confirm's start runs start_daemon"
    );
    app.modals.pop();
    super::run_confirm_action(&mut app, ConfirmAction::StartDaemon);
    assert_eq!(
        app.toasts.back().map(|t| t.body.as_str()),
        Some("daemon start failed\na test build starts no daemon")
    );
    assert!(
        !app.daemon_control_busy,
        "a refused start leaves the verb armed"
    );

    dispatch_action_menu_action(&mut app, ActionMenuAction::StopDaemon);
    assert!(
        app.daemon_control_busy,
        "the stop holds the verb back while it runs"
    );
    assert_eq!(
        app.toasts.back().map(|t| t.body.as_str()),
        Some("stopping daemon")
    );
    join_test_workers();
    super::on_tick(&mut app);
    assert!(
        app.toasts.iter().any(|t| t.body == "no daemon was running"),
        "the tick drains the worker's outcome"
    );
    assert!(!app.daemon_control_busy);
}

// ── C5: the `a`-menu shunt verb + the shared daemon-start confirm ────────────

/// Pin the app's daemon health and re-read the gateway slot under it, as the
/// throttled poll does.
fn set_daemon(app: &mut App, health: crate::daemon::DaemonHealth) {
    app.daemon_health = health;
    super::sync_gateway_state(app);
}

/// Drain the Services probes until the shunt action in flight lands.
fn drain_shunt_action(app: &mut App) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.services.shunt_action.running {
        assert!(
            std::time::Instant::now() < deadline,
            "the shunt action did not land"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_service_probes(app);
    }
    super::drain_service_probes(app);
}

/// The confirm on top, or a panic naming what is open instead.
fn top_confirm(app: &App) -> &super::ConfirmState {
    match app.modals.last() {
        Some(super::Modal::Confirm(state)) => state,
        other => panic!("expected a confirm, got {other:?}"),
    }
}

/// The verb table, first match wins: a running gateway under a fresh daemon
/// stops; an absent or disabled gateway jumps; a no-daemon box opens the
/// daemon-start confirm; a held gateway releases; the rest offer nothing. All
/// through `build_action_menu`.
#[test]
fn the_shunt_verb_table_holds_per_state_and_health() {
    use super::build_action_menu;
    use crate::daemon::DaemonHealth;
    use crate::daemon::gateway::GatewayState as S;
    use crate::profile::Profile;

    let cases: &[(DaemonHealth, S, Option<&str>)] = &[
        // Running gateway under a fresh daemon: stop (the hold).
        (DaemonHealth::Fresh, S::Starting, Some("stop shunt")),
        (DaemonHealth::Fresh, S::Healthy, Some("stop shunt")),
        (DaemonHealth::Fresh, S::Unhealthy, Some("stop shunt")),
        (DaemonHealth::Fresh, S::Restarting, Some("stop shunt")),
        // Absent: jump to the adopt line, any health.
        (DaemonHealth::Fresh, S::Absent, Some("start shunt")),
        (DaemonHealth::Stale, S::Absent, Some("start shunt")),
        (DaemonHealth::Absent, S::Absent, Some("start shunt")),
        // Disabled: jump to the enabled row, any health.
        (DaemonHealth::Fresh, S::Disabled, Some("start shunt")),
        (DaemonHealth::Stale, S::Disabled, Some("start shunt")),
        (DaemonHealth::Absent, S::Disabled, Some("start shunt")),
        // No daemon, whatever the record: the daemon-start confirm.
        (DaemonHealth::Absent, S::Unobserved, Some("start shunt")),
        (DaemonHealth::Absent, S::NoConfig, Some("start shunt")),
        (DaemonHealth::Absent, S::YamlRefused, Some("start shunt")),
        (DaemonHealth::Absent, S::Misconfigured, Some("start shunt")),
        // Held: release.
        (DaemonHealth::Fresh, S::Held, Some("start shunt")),
        // The rest offer nothing.
        (DaemonHealth::Fresh, S::Foreign, None),
        (DaemonHealth::Fresh, S::BelowFloor, None),
        (DaemonHealth::Fresh, S::Misconfigured, None),
        (DaemonHealth::Fresh, S::Stopping, None),
        (DaemonHealth::Fresh, S::NoConfig, None),
        (DaemonHealth::Fresh, S::YamlRefused, None),
        (DaemonHealth::Fresh, S::BinaryMissing, None),
        (DaemonHealth::Fresh, S::Unobserved, None),
        (DaemonHealth::Stale, S::Unobserved, None),
        (DaemonHealth::Stale, S::NoConfig, None),
        (DaemonHealth::Stale, S::YamlRefused, None),
        (DaemonHealth::Stale, S::Misconfigured, None),
    ];

    for &(health, state, expected) in cases {
        let home = crate::testutil::HomeSandbox::new();
        match health {
            DaemonHealth::Fresh => crate::testutil::write_daemon_gateway_slot(&home, state),
            DaemonHealth::Stale | DaemonHealth::Absent => match state {
                S::Absent => {}
                S::Disabled => {
                    crate::testutil::write_adopted_record(&home, true, "[server]\n");
                }
                S::Unobserved => {
                    crate::testutil::write_adopted_record(&home, false, "[server]\n");
                }
                S::NoConfig => {
                    let record = crate::testutil::write_adopted_record(&home, false, "[server]\n");
                    std::fs::remove_file(record.config()).unwrap();
                }
                // A record naming a YAML config, and one that does not parse.
                S::YamlRefused | S::Misconfigured => {
                    let dir = home.home().join(".clauth");
                    std::fs::create_dir_all(&dir).unwrap();
                    let yaml = home.home().join("etc").join("shunt.yaml");
                    let body = if state == S::YamlRefused {
                        format!("config = {:?}\n", yaml.display().to_string())
                    } else {
                        "config = 3\n".to_string()
                    };
                    std::fs::write(dir.join("gateway.toml"), body).unwrap();
                }
                other => panic!("unreachable record-only state {other:?}"),
            },
        }
        let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
        app.profile_cursor = 0;
        set_daemon(&mut app, health);
        assert_eq!(app.gateway_state, state, "the fixture reads as {state:?}");
        let shunt_verbs: Vec<&str> = build_action_menu(&app)
            .items
            .iter()
            .map(|i| i.label)
            .filter(|l| l.ends_with(" shunt"))
            .collect();
        let expected: Vec<&str> = expected.into_iter().collect();
        assert_eq!(
            shunt_verbs, expected,
            "{health:?} with the gateway {state:?}"
        );
    }
}

/// The shunt verb sits directly after the daemon verb on every tab, and is
/// gone while a shunt action is in flight; the daemon-start row also yields
/// none while a daemon control runs.
#[test]
fn the_shunt_verb_follows_the_daemon_verb_and_gates_on_in_flight() {
    use super::{Tab, build_action_menu};
    use crate::daemon::DaemonHealth;
    use crate::daemon::gateway::GatewayState as S;
    use crate::profile::Profile;

    // A fresh daemon running a healthy gateway: stop shunt after stop daemon.
    {
        let home = crate::testutil::HomeSandbox::new();
        crate::testutil::write_daemon_gateway_slot(&home, S::Healthy);
        let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
        app.profile_cursor = 0;
        set_daemon(&mut app, DaemonHealth::Fresh);
        for tab in Tab::ALL {
            app.tab = tab;
            let labels: Vec<&str> = build_action_menu(&app)
                .items
                .iter()
                .map(|i| i.label)
                .collect();
            let daemon_idx = labels
                .iter()
                .position(|l| l.ends_with(" daemon"))
                .unwrap_or_else(|| panic!("{tab:?}: a daemon verb"));
            assert_eq!(
                labels.get(daemon_idx + 1).copied(),
                Some("stop shunt"),
                "{tab:?}: stop shunt directly after stop daemon"
            );
        }

        // In flight: no shunt verb while a shunt action runs.
        app.tab = Tab::Overview;
        app.services.shunt_action.running = true;
        assert!(
            !build_action_menu(&app)
                .items
                .iter()
                .any(|i| i.label.ends_with(" shunt")),
            "no shunt verb while a shunt action runs"
        );
        app.services.shunt_action.running = false;
    }

    // The daemon-start row yields none while a daemon control runs.
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_adopted_record(&home, false, "[server]\n");
    let mut app = app_with(vec![Profile::new("acct".to_string(), None, None)]);
    set_daemon(&mut app, DaemonHealth::Absent);
    assert_eq!(app.gateway_state, S::Unobserved);
    app.daemon_control_busy = true;
    assert!(
        !build_action_menu(&app)
            .items
            .iter()
            .any(|i| i.label.ends_with(" shunt")),
        "the daemon-start row yields none while a daemon control runs"
    );
}

/// The one Services detail row carrying the caret after a jump, drawn the way
/// the TUI draws it.
fn caret_row(app: &App) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, app)).unwrap();
    let rows: Vec<String> = crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .filter(|row| row.contains('❯'))
        .collect();
    assert_eq!(rows.len(), 1, "one caret on screen: {rows:#?}");
    rows.into_iter().next().unwrap_or_default()
}

/// `start shunt` on an absent gateway jumps to the Services shunt card with the
/// `f  adopt config` line focused and its caret drawn; a card with no adopt
/// line (a YAML find) lands with the descend rule's focus (none).
#[test]
fn start_shunt_on_an_absent_gateway_jumps_to_the_adopt_line() {
    use super::{
        ActionMenuAction, ServicesFocus, ShuntFocusLine, StandaloneShunt, Tab,
        dispatch_action_menu_action,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.services.standalone = Some(StandaloneShunt {
        found: Some(std::path::PathBuf::from("/cfg/shunt.toml")),
        unread_bind: None,
        unread_config: None,
        unread_env: false,
        answer: None,
    });
    dispatch_action_menu_action(&mut app, ActionMenuAction::StartShunt);

    assert_eq!(app.tab, Tab::Services, "lands on the Services tab");
    let shunt_idx = app
        .services
        .checks
        .iter()
        .position(|c| c.label == "shunt")
        .expect("a shunt row");
    assert_eq!(app.services.cursor, shunt_idx, "the shunt row selected");
    assert_eq!(app.services.focus, ServicesFocus::Detail, "descended");
    assert_eq!(
        app.services.shunt_focus.get(),
        Some(ShuntFocusLine::Adopt),
        "the adopt line focused"
    );
    let row = caret_row(&app);
    assert!(
        row.contains("❯ f  adopt config"),
        "the caret sits on the adopt line: {row:?}"
    );

    // A YAML find offers no adopt line: the focus is the descend rule's (none).
    let mut app = bare_app();
    app.services.standalone = Some(StandaloneShunt {
        found: Some(std::path::PathBuf::from("/cfg/shunt.yaml")),
        unread_bind: Some(super::UnreadBind::Yaml),
        unread_config: Some(super::UnreadConfig::Yaml),
        unread_env: false,
        answer: None,
    });
    dispatch_action_menu_action(&mut app, ActionMenuAction::StartShunt);
    assert_eq!(app.tab, Tab::Services);
    assert_eq!(app.services.focus, ServicesFocus::Detail, "still descended");
    assert_eq!(
        app.services.shunt_focus.get(),
        None,
        "no adopt line: the descend rule leaves focus none"
    );
}

/// `start shunt` on a disabled gateway jumps to the `enabled` row, caret drawn,
/// and never flips the record.
#[test]
fn start_shunt_on_a_disabled_gateway_jumps_to_the_enabled_row_without_flipping_it() {
    use super::{ActionMenuAction, ServicesFocus, ShuntFocusLine, dispatch_action_menu_action};
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_adopted_record(&home, true, "[server]\n");
    let mut app = bare_app();

    dispatch_action_menu_action(&mut app, ActionMenuAction::StartShunt);
    assert_eq!(app.tab, super::Tab::Services);
    let shunt_idx = app
        .services
        .checks
        .iter()
        .position(|c| c.label == "shunt")
        .expect("a shunt row");
    assert_eq!(app.services.cursor, shunt_idx, "the shunt row selected");
    assert_eq!(app.services.focus, ServicesFocus::Detail, "descended");
    assert_eq!(
        app.services.shunt_focus.get(),
        Some(ShuntFocusLine::Enabled),
        "the enabled row focused"
    );
    let row = caret_row(&app);
    assert!(
        row.contains("❯ enabled"),
        "the caret sits on the enabled row: {row:?}"
    );
    assert!(
        crate::gateway::GatewayRecord::load()
            .unwrap()
            .unwrap()
            .disabled,
        "the record is untouched"
    );
}

/// A no-daemon `start shunt` opens the shared daemon-start confirm; `esc` and
/// `enter` on the focused `cancel` close it without reaching `start_daemon`,
/// and `start` reaches it (which a test build refuses, by name).
#[test]
fn a_no_daemon_start_shunt_opens_the_daemon_start_confirm() {
    use super::{
        ActionMenuAction, ConfirmAction, KeyCode, dispatch_action_menu_action, handle_confirm_key,
    };
    const REFUSED: &str = "daemon start failed\na test build starts no daemon";
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_adopted_record(&home, false, "[server]\n");
    let mut app = bare_app();
    let open = |app: &mut App| {
        dispatch_action_menu_action(app, ActionMenuAction::StartShunt);
        let state = top_confirm(app);
        assert_eq!(
            (
                state.message.as_str(),
                state.detail.as_deref(),
                state.choice
            ),
            ("start daemon?", None, false),
            "the shared confirm, cancel focused"
        );
        assert!(matches!(state.on_confirm, ConfirmAction::StartDaemon));
    };

    open(&mut app);
    handle_confirm_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(app.modals.is_empty(), "esc closes the confirm");
    assert!(
        app.toasts.iter().all(|t| t.body != REFUSED),
        "esc never reaches start_daemon: {:?}",
        app.toasts
    );

    open(&mut app);
    handle_confirm_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(app.modals.is_empty(), "cancel closes the confirm");
    assert!(
        app.toasts.iter().all(|t| t.body != REFUSED),
        "cancel never reaches start_daemon: {:?}",
        app.toasts
    );

    open(&mut app);
    handle_confirm_key(&mut app, crate::testutil::key(KeyCode::Char('y')));
    handle_confirm_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(
        app.toasts.back().map(|t| t.body.as_str()),
        Some(REFUSED),
        "start reaches start_daemon"
    );
}

/// Through the menu: `stop shunt` opens its confirm, whose `stop` writes the
/// hold naming this process (a held daemon lock + pid stamp); once the feed
/// reports `held`, `start shunt` removes it with no confirm.
#[test]
fn the_shunt_hold_verbs_write_and_remove_the_hold_through_the_menu() {
    use super::{ActionMenuAction, ConfirmAction, KeyCode, ToastKind, dispatch_action_menu_action};
    use crate::daemon::DaemonHealth;
    use crate::daemon::gateway::GatewayState as S;
    let home = crate::testutil::HomeSandbox::new();
    let _held = crate::daemon::hold_daemon_lock();
    std::fs::write(
        home.home().join(".clauth").join("clauthd.pid"),
        format!("{}\n", std::process::id()),
    )
    .unwrap();
    crate::testutil::write_daemon_gateway_slot(&home, S::Healthy);

    let mut app = bare_app();
    app.services.shunt_action.prober = Some(super::run_shunt_action);
    set_daemon(&mut app, DaemonHealth::Fresh);

    dispatch_action_menu_action(&mut app, ActionMenuAction::StopShunt);
    let state = top_confirm(&app);
    assert_eq!(state.message, "stop shunt?");
    assert!(matches!(state.on_confirm, ConfirmAction::StopShunt));
    assert!(
        !app.services.shunt_action.running,
        "nothing runs before the confirm's stop"
    );
    super::handle_confirm_key(&mut app, crate::testutil::key(KeyCode::Char('y')));
    super::handle_confirm_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(app.services.shunt_action.running, "stop starts the hold");
    drain_shunt_action(&mut app);
    let hold = crate::gateway::hold_path().expect("hold path");
    let identity: crate::gateway::DaemonIdentity =
        serde_json::from_slice(&std::fs::read(&hold).unwrap()).unwrap();
    assert_eq!(
        identity.pid,
        std::process::id(),
        "the hold names this process"
    );
    assert_eq!(
        identity.start,
        crate::daemon::gateway::process_start_time(std::process::id()),
        "and this process start time"
    );
    assert_eq!(
        app.toasts.back().map(|t| (t.kind, t.body.as_str())),
        Some((
            ToastKind::Success,
            "shunt stopped\nit stays off until the daemon restarts"
        ))
    );

    crate::testutil::write_daemon_gateway_slot(&home, S::Held);
    dispatch_action_menu_action(&mut app, ActionMenuAction::StartShunt);
    assert!(app.modals.is_empty(), "a release opens no confirm");
    assert!(
        app.services.shunt_action.running,
        "start starts the release"
    );
    drain_shunt_action(&mut app);
    assert!(!hold.exists(), "the hold file is removed");
    assert_eq!(
        app.toasts.back().map(|t| (t.kind, t.body.as_str())),
        Some((ToastKind::Success, "shunt starting"))
    );
}

/// A pick re-derives the verb from the slot at pick time: one whose label
/// changed under the open menu, or that vanished, toasts and runs nothing,
/// the stop confirm's `stop` included; a `start shunt` whose effect changed
/// acts on the new state.
#[test]
fn a_shunt_pick_acts_on_the_state_at_pick_time() {
    use super::{
        ActionMenuAction, KeyCode, ServicesFocus, ShuntFocusLine, ToastKind,
        dispatch_action_menu_action,
    };
    use crate::daemon::DaemonHealth;
    use crate::daemon::gateway::GatewayState as S;
    const CHANGED: (ToastKind, &str) = (
        ToastKind::Info,
        "the shunt gateway changed; open the menu again",
    );
    let last_toast = |app: &App| app.toasts.back().map(|t| (t.kind, t.body.clone()));
    let changed_owned = Some((CHANGED.0, CHANGED.1.to_string()));

    {
        let home = crate::testutil::HomeSandbox::new();
        crate::testutil::write_daemon_gateway_slot(&home, S::Healthy);
        let mut app = bare_app();
        app.services.shunt_action.prober = Some(super::run_shunt_action);
        set_daemon(&mut app, DaemonHealth::Fresh);

        // `stop shunt` picked while the gateway went `stopping`: no verb now.
        crate::testutil::write_daemon_gateway_slot(&home, S::Stopping);
        dispatch_action_menu_action(&mut app, ActionMenuAction::StopShunt);
        assert!(app.modals.is_empty(), "no confirm for a vanished verb");
        assert!(!app.services.shunt_action.running, "nothing runs");
        assert_eq!(last_toast(&app), changed_owned);

        // `start shunt` (the release) picked while the gateway runs again: the
        // verb is `stop shunt` now.
        crate::testutil::write_daemon_gateway_slot(&home, S::Healthy);
        app.toasts.clear();
        dispatch_action_menu_action(&mut app, ActionMenuAction::StartShunt);
        assert!(app.modals.is_empty());
        assert!(!app.services.shunt_action.running, "nothing runs");
        assert_eq!(last_toast(&app), changed_owned);

        // The stop confirm's `stop` re-checks: the gateway went `stopping`
        // while it sat open.
        dispatch_action_menu_action(&mut app, ActionMenuAction::StopShunt);
        assert_eq!(top_confirm(&app).message, "stop shunt?");
        crate::testutil::write_daemon_gateway_slot(&home, S::Stopping);
        app.toasts.clear();
        super::handle_confirm_key(&mut app, crate::testutil::key(KeyCode::Char('y')));
        super::handle_confirm_key(&mut app, crate::testutil::key(KeyCode::Enter));
        assert!(!app.services.shunt_action.running, "no hold is written");
        assert_eq!(last_toast(&app), changed_owned);
    }

    // `start shunt` offered for an absent gateway, picked once the record is
    // adopted `disabled`: it jumps to the `enabled` row, the state at pick time.
    let home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    set_daemon(&mut app, DaemonHealth::Absent);
    assert_eq!(app.gateway_state, S::Absent);
    crate::testutil::write_adopted_record(&home, true, "[server]\n");
    dispatch_action_menu_action(&mut app, ActionMenuAction::StartShunt);
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    assert_eq!(
        app.services.shunt_focus.get(),
        Some(ShuntFocusLine::Enabled),
        "the jump the new state names"
    );
    assert!(
        app.toasts
            .iter()
            .all(|t| (t.kind, t.body.as_str()) != CHANGED),
        "the same label acts without a toast: {:?}",
        app.toasts
    );
}

/// `stop shunt?` leads its detail with the live sessions whose current account's
/// `base_url` names the gateway's bind, singular at 1; the clause is gone at 0,
/// and when the tally or the gateway record cannot be read.
#[test]
fn the_stop_confirm_counts_the_sessions_on_the_gateway() {
    use crate::live_sessions::LiveTally;
    use crate::profile::Profile;
    use crate::testutil::live_row;
    const DRAIN: &str =
        "in-flight requests drain, then the gateway stays off until the daemon restarts.";
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_adopted_record(&home, false, "[server]\nbind = \"127.0.0.1:3067\"\n");
    let profile = |name: &str, url: Option<&str>| {
        Profile::new(name.to_string(), url.map(str::to_string), None)
    };
    let mut app = app_with(vec![
        profile("gw", Some("http://127.0.0.1:3067")),
        profile("gw-local", Some("http://localhost:3067/")),
        profile("other-port", Some("http://127.0.0.1:4000")),
        profile("plain", None),
    ]);
    let detail = |app: &mut App, tally: LiveTally| {
        app.live_sessions = tally;
        app.modals.clear();
        super::open_shunt_stop_confirm(app);
        let state = top_confirm(app);
        assert_eq!(state.message, "stop shunt?");
        state.detail.clone()
    };

    let mut swapped_off = live_row("s4", "gw");
    swapped_off.current_member = Some("plain".to_string());
    let mut swapped_on = live_row("s5", "plain");
    swapped_on.current_member = Some("gw".to_string());
    assert_eq!(
        detail(
            &mut app,
            LiveTally::of([
                live_row("s1", "gw"),
                live_row("s2", "gw-local"),
                live_row("s3", "other-port"),
                swapped_off,
                swapped_on,
            ])
        ),
        Some(format!("3 live sessions use it; {DRAIN}")),
        "s1, s2 and s5 run on the gateway now; s3 and s4 do not"
    );
    assert_eq!(
        detail(&mut app, LiveTally::of([live_row("s1", "gw")])),
        Some(format!("1 live session uses it; {DRAIN}"))
    );
    assert_eq!(
        detail(&mut app, LiveTally::of([live_row("s3", "other-port")])),
        Some(DRAIN.to_string())
    );
    assert_eq!(
        detail(&mut app, LiveTally::partly_read([live_row("s1", "gw")])),
        Some(DRAIN.to_string()),
        "a tally that could not read every dir shows no count"
    );

    // The port is the running gateway's: a bind edited to 4000 since its spawn
    // moves nothing until a restart, so the slot's 3067 still counts.
    let record = crate::gateway::GatewayRecord::load().unwrap().unwrap();
    std::fs::write(record.config(), "[server]\nbind = \"127.0.0.1:4000\"\n").unwrap();
    let feed = serde_json::json!({
        "gateway": {
            "state": crate::daemon::gateway::GatewayState::Healthy,
            "port": 3067,
            "floor": "0.48.0",
            "restarts": 0,
        }
    });
    std::fs::write(
        home.home().join(".clauth").join("status.json"),
        feed.to_string(),
    )
    .unwrap();
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    assert_eq!(
        detail(&mut app, LiveTally::of([live_row("s1", "gw")])),
        Some(format!("1 live session uses it; {DRAIN}")),
        "the slot's port, never the edited config's"
    );
    app.daemon_health = crate::daemon::DaemonHealth::Absent;
    std::fs::remove_file(home.home().join(".clauth").join("gateway.toml")).unwrap();
    assert_eq!(
        detail(&mut app, LiveTally::of([live_row("s1", "gw")])),
        Some(DRAIN.to_string()),
        "no record, no bind to count against"
    );
}

/// The cached gateway state is read at construction and follows the slot on
/// the throttled daemon poll, on a landed daemon control and on a Services
/// recompute, so the menu never reads a state the disk left.
#[test]
fn the_cached_gateway_state_follows_every_sync_site() {
    use crate::daemon::gateway::GatewayState as S;
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_adopted_record(&home, true, "[server]\n");
    let mut app = bare_app();
    assert_eq!(app.gateway_state, S::Disabled, "construction read the slot");

    let set_disabled = |disabled: bool| {
        crate::gateway::GatewayRecord::update(|slot| {
            if let Some(record) = slot.as_mut() {
                record.disabled = disabled;
            }
            Ok(())
        })
        .unwrap();
    };

    set_disabled(false);
    app.last_daemon_probe = std::time::Instant::now() - std::time::Duration::from_secs(2);
    super::poll_daemon_health(&mut app);
    assert_eq!(
        app.gateway_state,
        S::Unobserved,
        "the poll re-read the slot"
    );

    set_disabled(true);
    app.daemon_control_tx
        .send(super::DaemonControlResult::Stop(Ok(
            crate::daemon::DaemonStop::NotRunning,
        )))
        .expect("send");
    super::drain_daemon_control(&mut app);
    assert_eq!(
        app.gateway_state,
        S::Disabled,
        "a landed daemon control re-read the slot"
    );

    set_disabled(false);
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert_eq!(
        app.gateway_state,
        S::Unobserved,
        "a Services recompute re-read the slot"
    );
}

/// The no-daemon warning's toast, copy spelled out with the `a` key's marker.
const GATEWAY_UNRUN_TOAST: &str =
    "the shunt gateway will not run\nno daemon runs · \u{E000}a\u{E002} → start daemon";

/// The same warning's copy while a gateway the last daemon left still serves.
const GATEWAY_LEFT_RUNNING_TOAST: &str = "a shunt gateway the last daemon left still runs\nno daemon supervises it · \u{E000}a\u{E002} → start daemon";

fn gateway_unrun_toasts(app: &App) -> Vec<super::ToastKind> {
    app.toasts
        .iter()
        .filter(|t| t.body == GATEWAY_UNRUN_TOAST)
        .map(|t| t.kind)
        .collect()
}

/// Stub the left-behind probe (`None` under test) with a fixed answer.
fn stub_left_behind(app: &mut App, left_running: bool) {
    app.left_behind_probe.prober = Some(if left_running { || true } else { || false });
}

/// Drain the left-behind probe until its run lands.
fn await_left_behind(app: &mut App) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.left_behind_probe.running {
        assert!(
            std::time::Instant::now() < deadline,
            "the left-behind probe did not land within 10 s"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_left_behind_probe(app);
    }
}

/// The throttled daemon poll, forced due, then the probe it may start.
fn poll_and_land(app: &mut App) {
    app.last_daemon_probe = std::time::Instant::now() - std::time::Duration::from_secs(2);
    super::poll_daemon_health(app);
    await_left_behind(app);
}

fn set_gateway_disabled(disabled: bool) {
    crate::gateway::GatewayRecord::update(|slot| {
        if let Some(record) = slot.as_mut() {
            record.disabled = disabled;
        }
        Ok(())
    })
    .unwrap();
}

/// A gateway adopted and enabled with no daemon to run it toasts once, amber,
/// on entering that state: a re-probe in the same state stays quiet, and
/// leaving it (the record disabled) then entering it again toasts again.
#[test]
fn a_gateway_with_no_daemon_to_run_it_toasts_once_per_entry() {
    use crate::daemon::gateway::GatewayState as S;
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_adopted_record(&home, false, "[server]\n");
    let mut app = bare_app();
    stub_left_behind(&mut app, false);

    poll_and_land(&mut app);
    assert_eq!(app.daemon_health, crate::daemon::DaemonHealth::Absent);
    assert_eq!(
        app.gateway_state,
        S::Unobserved,
        "adopted, enabled, no daemon"
    );
    assert_eq!(
        gateway_unrun_toasts(&app),
        vec![super::ToastKind::Warning],
        "entering the state toasts once, amber"
    );

    poll_and_land(&mut app);
    assert_eq!(
        gateway_unrun_toasts(&app).len(),
        1,
        "a re-probe in the same state stays quiet"
    );

    set_gateway_disabled(true);
    poll_and_land(&mut app);
    assert_eq!(app.gateway_state, S::Disabled);
    assert_eq!(
        gateway_unrun_toasts(&app).len(),
        1,
        "a disabled gateway is not warned about"
    );

    set_gateway_disabled(false);
    poll_and_land(&mut app);
    assert_eq!(
        gateway_unrun_toasts(&app).len(),
        2,
        "entering the state again toasts again"
    );
}

/// A daemon that holds the singleton but publishes a stale feed still runs its
/// gateway's supervisor, so the record-only `unobserved` read under it is not
/// the no-daemon state and raises no warning; that daemon then dying is.
#[test]
fn a_stale_daemon_raises_no_gateway_warning() {
    use crate::daemon::gateway::GatewayState as S;
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_adopted_record(&home, false, "[server]\n");
    let mut app = bare_app();
    stub_left_behind(&mut app, false);
    app.toasts.clear();
    app.daemon_health = crate::daemon::DaemonHealth::Stale;
    super::sync_gateway_state(&mut app);
    await_left_behind(&mut app);
    assert_eq!(app.gateway_state, S::Unobserved);
    assert!(
        gateway_unrun_toasts(&app).is_empty(),
        "a stale daemon raised {} warning(s)",
        gateway_unrun_toasts(&app).len()
    );

    app.daemon_health = crate::daemon::DaemonHealth::Absent;
    super::sync_gateway_state(&mut app);
    await_left_behind(&mut app);
    assert_eq!(
        gateway_unrun_toasts(&app),
        vec![super::ToastKind::Warning],
        "a stale daemon going away enters the no-daemon state"
    );
}

/// The probe's answer picks the copy: a gateway the last daemon left still
/// serving swaps it, none keeps the plain one.
#[test]
fn the_no_daemon_warning_says_a_left_behind_gateway_still_runs() {
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_adopted_record(&home, false, "[server]\n");
    for (left_running, expected) in [
        (true, GATEWAY_LEFT_RUNNING_TOAST),
        (false, GATEWAY_UNRUN_TOAST),
    ] {
        let mut app = bare_app();
        stub_left_behind(&mut app, left_running);
        app.toasts.clear();
        poll_and_land(&mut app);
        let bodies: Vec<&str> = app.toasts.iter().map(|t| t.body.as_str()).collect();
        assert_eq!(bodies, vec![expected], "left_running = {left_running}");
    }
}

/// Production turns the probe's landing into the toast on the tick path:
/// ticking alone, with no direct drain, lands the no-daemon warning once.
#[test]
fn the_tick_lands_the_no_daemon_warning() {
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_adopted_record(&home, false, "[server]\n");
    let mut app = bare_app();
    stub_left_behind(&mut app, false);
    app.toasts.clear();
    app.last_daemon_probe = std::time::Instant::now() - std::time::Duration::from_secs(2);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while gateway_unrun_toasts(&app).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "ticking never landed the no-daemon warning"
        );
        super::on_tick(&mut app);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(gateway_unrun_toasts(&app), vec![super::ToastKind::Warning]);
}

/// Toast lifetimes per the cloudy-tui contract: a red toast lingers 6 s, every
/// other kind 5 s, each pinned on both sides of its edge.
#[test]
fn toasts_live_five_seconds_and_red_ones_six() {
    use super::ToastKind as K;
    use std::time::{Duration, Instant};
    let _home = crate::testutil::HomeSandbox::new();
    for (kind, ttl_ms) in [
        (K::Info, 5_000),
        (K::Success, 5_000),
        (K::Warning, 5_000),
        (K::Danger, 6_000),
    ] {
        for (age_ms, kept) in [(ttl_ms - 100, true), (ttl_ms, false)] {
            let mut app = bare_app();
            app.toasts.clear();
            app.toast(kind, "a toast");
            app.toasts.back_mut().expect("the toast").born =
                Instant::now() - Duration::from_millis(age_ms);
            app.prune_toasts();
            assert_eq!(
                app.toasts.len(),
                usize::from(kept),
                "{kind:?} at {age_ms} ms: kept = {kept}"
            );
        }
    }
}

/// Each toast expires on its own clock: a live red toast at the front of the
/// stack never holds an expired one behind it on screen.
#[test]
fn an_expired_toast_behind_a_live_red_one_still_expires() {
    use super::ToastKind as K;
    use std::time::{Duration, Instant};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.toasts.clear();
    for (kind, age_ms) in [(K::Danger, 5_500), (K::Info, 5_200), (K::Warning, 1_000)] {
        app.toast(kind, format!("{kind:?}"));
        app.toasts.back_mut().expect("the toast").born =
            Instant::now() - Duration::from_millis(age_ms);
    }
    app.prune_toasts();
    let kinds: Vec<K> = app.toasts.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        vec![K::Danger, K::Warning],
        "the 5.2 s info toast goes, the 5.5 s red and 1 s warning ones stay, in order"
    );
}

/// A probe landing after the app left the state it started in raises nothing:
/// the warning is about a state that no longer holds.
#[test]
fn a_left_behind_probe_landing_after_the_state_left_raises_nothing() {
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_adopted_record(&home, false, "[server]\n");
    let mut app = bare_app();
    stub_left_behind(&mut app, false);
    app.toasts.clear();
    app.last_daemon_probe = std::time::Instant::now() - std::time::Duration::from_secs(2);
    super::poll_daemon_health(&mut app);
    assert!(app.left_behind_probe.running, "the entry started the probe");

    set_gateway_disabled(true);
    super::sync_gateway_state(&mut app);
    assert!(!app.gateway_unrun, "the record disabled leaves the state");
    await_left_behind(&mut app);
    assert!(
        app.toasts.is_empty(),
        "no toast for a state already left: {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );
}

/// The hold's success and failure toasts, kind and body.
#[test]
fn the_shunt_hold_toasts_follow_the_outcome() {
    use super::{ShuntActionOutcome as O, ToastKind};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    for (outcome, kind, body) in [
        (
            O::Held,
            ToastKind::Success,
            "shunt stopped\nit stays off until the daemon restarts",
        ),
        (
            O::HoldFailed {
                error: "boom".to_string(),
            },
            ToastKind::Danger,
            "shunt stop failed\nboom",
        ),
        (O::Released, ToastKind::Success, "shunt starting"),
        (
            O::ReleaseFailed {
                error: "boom".to_string(),
            },
            ToastKind::Danger,
            "shunt start failed\nboom",
        ),
    ] {
        app.toasts.clear();
        super::handle_shunt_action_outcome(&mut app, outcome);
        assert_eq!(
            app.toasts
                .iter()
                .map(|t| (t.kind, t.body.as_str()))
                .collect::<Vec<_>>(),
            [(kind, body)]
        );
    }
}

/// Usage `r` and the action menu's "refresh usage" share one gate. A generic
/// api-key endpoint (litellm: `provider` is `None`, so `is_third_party` is
/// false) is fetched every cadence by the same leg a recognised provider runs
/// on, so its row queues the same refetch instead of the nothing-to-refresh
/// toast. The OAuth and recognised-provider arms stay as they were; only an
/// endpoint with no credential either leg can use (keyless, no pair) keeps the
/// toast.
#[test]
fn usage_refresh_queues_a_generic_api_key_account() {
    use super::{ActionMenuAction, KeyCode, Tab, dispatch_action_menu_action, handle_key};
    use crate::profile::Profile;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(vec![
        Profile::new("oauth".to_string(), None, None),
        Profile::new(
            "vendor".to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            Some("sk-fixture".to_string()),
        ),
        Profile::new(
            "litellm".to_string(),
            Some("http://127.0.0.1:4000".to_string()),
            Some("sk-fixture".to_string()),
        ),
        Profile::new(
            "keyless".to_string(),
            Some("http://127.0.0.1:9999".to_string()),
            None,
        ),
    ]);
    app.tab = Tab::Usage;

    let queued = |app: &App| -> Vec<String> {
        app.refetch_queue
            .lock()
            .expect("queue lock")
            .iter()
            .cloned()
            .collect()
    };
    let last_toast = |app: &App| -> String {
        app.toasts
            .back()
            .map(|t| t.body.clone())
            .unwrap_or_default()
    };

    // The generic row the fix is about: queued, not toasted away.
    app.profile_cursor = 2;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('r')));
    assert!(
        queued(&app).contains(&"litellm".to_string()),
        "the third-party leg fetches this account, so `r` queues it: {:?}",
        queued(&app)
    );
    assert_eq!(last_toast(&app), "refreshing 'litellm'");

    // The two arms that must not move.
    app.profile_cursor = 0;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('r')));
    assert!(
        queued(&app).contains(&"oauth".to_string()),
        "the OAuth arm queues as before: {:?}",
        queued(&app)
    );
    assert_eq!(last_toast(&app), "refreshing 'oauth'");

    app.profile_cursor = 1;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('r')));
    assert!(
        queued(&app).contains(&"vendor".to_string()),
        "the recognised-provider arm queues as before: {:?}",
        queued(&app)
    );
    assert_eq!(last_toast(&app), "refreshing 'vendor'");

    // A keyless endpoint has no credential either leg can fetch with: the
    // toast arm is unchanged for it.
    app.profile_cursor = 3;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('r')));
    assert!(
        !queued(&app).contains(&"keyless".to_string()),
        "a keyless endpoint queues nothing: {:?}",
        queued(&app)
    );
    assert_eq!(last_toast(&app), "'keyless' has no usage to refresh");

    // The menu entry rides the same gate.
    app.profile_cursor = 2;
    dispatch_action_menu_action(&mut app, ActionMenuAction::RefreshUsage);
    assert_eq!(last_toast(&app), "refreshing 'litellm'");
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
    let _pid_guard = crate::testutil::arm_live_session(home.home(), "busy");

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
    let _pid_guard = crate::testutil::arm_live_session(home.home(), "busy");

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
    let _pid_guard = crate::testutil::arm_live_session(home.home(), "busy");

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
    let _pid_guard = crate::testutil::arm_live_session(home.home(), "busy");

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
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// A completed login as `PendingLogin::run` hands it back: the mint plus the account
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
            ..crate::profile::OAuthToken::default_extra()
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

/// An in-flight login session fixture at the waiting stage, shaped like the
/// console login: a browser and no paste door.
fn login_session(name: &str, is_new: bool, generation: u64) -> super::LoginSession {
    super::LoginSession {
        name: name.to_string(),
        is_new,
        generation,
        url: None,
        stage: super::LoginStage::WaitingBrowser,
        method: super::LoginMethod::Browser,
        paste: None,
        paste_field: None,
    }
}

/// The `state` every paste-door fixture below is minted with.
const PASTE_STATE: &str = "fixture-state-7f3a";

/// An in-flight OAuth login at the waiting stage with its paste door open, plus
/// the receiver a worker would hold, so a test can see what a paste delivered.
/// The links are built by hand: binding a listener is `begin_login`'s job and
/// nothing here needs one.
fn paste_session(
    name: &str,
    is_new: bool,
    generation: u64,
) -> (
    super::LoginSession,
    std::sync::mpsc::Receiver<crate::oauth_login::ManualCode>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let session = super::LoginSession {
        name: name.to_string(),
        is_new,
        generation,
        url: Some(format!("http://localhost:1/authorize?state={PASTE_STATE}")),
        stage: super::LoginStage::WaitingBrowser,
        method: super::LoginMethod::Browser,
        paste: Some(super::PasteDoor {
            links: crate::oauth_login::LoginLinks {
                browser_url: format!("http://localhost:1/authorize?state={PASTE_STATE}"),
                hosted_url: format!("https://hosted.example/authorize?state={PASTE_STATE}"),
                state: PASTE_STATE.to_string(),
            },
            tx,
        }),
        paste_field: None,
    };
    (session, rx)
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
    use super::{
        ConfigFocus, ConfigRow, ConfirmAction, DraftLogin, Modal, build_draft_new, run_config_row,
    };
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
    draft.captured_login = Some(DraftLogin::Mint(Box::new(login_outcome(
        "stashed",
        Some("uuid-stashed"),
    ))));
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
    use super::{DraftLogin, build_draft_new, commit_new_account};
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
    draft.captured_login = Some(DraftLogin::Mint(Box::new(login_outcome(
        "minted",
        Some("uuid-minted"),
    ))));
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

// ── `+ capture current login` (the `+ new` form row) ─────────────────────────

/// ⏎ on `+ capture current login` stashes the live login into the draft like
/// `+ login` stashes its mint; `create account` then commits it under the
/// typed name, folding the typed model — the #72 flow, on the form.
#[test]
fn capture_row_stashes_and_create_account_commits() {
    use super::{
        ConfigFocus, ConfigRow, DraftLogin, ToastKind, build_draft_new, commit_new_account,
        config_rows, run_config_row,
    };
    let _home = crate::testutil::HomeSandbox::new();
    plain_live_login("live-refresh");
    let mut app = bare_app();
    app.refresh_unsaved_live_login();
    app.profile_cursor = 0; // the `+ new` form
    let mut draft = build_draft_new();
    draft.name = InputState::new("work");
    draft.model = InputState::new("sonnet");
    app.config_draft = Some(draft);
    app.config_focus = ConfigFocus::Actions;

    run_config_row(&mut app, ConfigRow::CaptureLogin);

    assert!(
        app.config()
            .find(&crate::profile::ProfileName::from("work"))
            .is_none(),
        "capture-then-commit: no profile until create fires"
    );
    assert!(
        matches!(
            app.config_draft
                .as_ref()
                .and_then(|d| d.captured_login.as_ref()),
            Some(DraftLogin::LiveLogin(_))
        ),
        "the live login lands in the draft"
    );
    assert_eq!(
        config_rows(&app).get(app.config_action_cursor),
        Some(&ConfigRow::Create),
        "the cursor lands on `create account`"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Success && t.body.contains("current login captured")),
        "the stash success toast names what happened"
    );

    commit_new_account(&mut app);

    let cfg = app.config();
    let profile = cfg
        .find(&crate::profile::ProfileName::from("work"))
        .expect("create account commits the captured login");
    assert_eq!(
        profile.refresh_token(),
        Some("live-refresh"),
        "the profile holds the live login's tokens"
    );
    assert_eq!(
        profile.models.default.as_deref(),
        Some("sonnet"),
        "the typed model folds into the same create"
    );
    assert!(
        !app.unsaved_live_login,
        "the flag drops once the created account owns the login"
    );
}

/// Ownership that appeared after the flag was computed: ⏎ refuses naming the
/// owner — a new account over an owned login is the duplicate the overwrite
/// path exists to prevent — and refreshes the flag so the row disappears.
#[test]
fn capture_row_over_an_owned_live_login_refuses() {
    use super::{ConfigFocus, ConfigRow, ToastKind, build_draft_new, run_config_row};
    let _home = crate::testutil::HomeSandbox::new();
    plain_live_login("rt-owner");
    let mut app = bare_app();
    {
        let mut cfg = app.config();
        cfg.profiles
            .push(stored_oauth_profile("owner", far_future()));
    }
    app.unsaved_live_login = true; // stale: the state the row was rendered on
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_new());
    app.config_focus = ConfigFocus::Actions;

    run_config_row(&mut app, ConfigRow::CaptureLogin);

    assert!(
        app.config_draft
            .as_ref()
            .is_some_and(|d| d.captured_login.is_none()),
        "nothing is stashed over an owned login"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Danger && t.body.contains("owner")),
        "the refusal names the owning profile"
    );
    assert!(
        !app.unsaved_live_login,
        "the refusal refreshes the flag the row renders on"
    );
}

/// The live file going empty between the flag and the press: the shared
/// `capture_live_or_toast` refusal, not a credential-less stash.
#[test]
fn capture_row_with_nothing_live_refuses() {
    use super::{ConfigFocus, ConfigRow, ToastKind, build_draft_new, run_config_row};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.unsaved_live_login = true; // stale
    app.profile_cursor = 0;
    app.config_draft = Some(build_draft_new());
    app.config_focus = ConfigFocus::Actions;

    run_config_row(&mut app, ConfigRow::CaptureLogin);

    assert!(
        app.config_draft
            .as_ref()
            .is_some_and(|d| d.captured_login.is_none()),
        "nothing is stashed from an empty live file"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Danger && t.body.contains("no live login found")),
        "the empty-snapshot refusal toast fires"
    );
}

/// A browser mint already stashed (`✓ logged in`): capturing over it asks
/// first, exactly like the re-login gate — the mint cost a real browser
/// round-trip. Confirming swaps the stash.
#[test]
fn capture_row_over_a_stashed_mint_confirms_first() {
    use super::{
        ConfigFocus, ConfigRow, ConfirmAction, DraftLogin, Modal, build_draft_new, run_config_row,
        run_confirm_action,
    };
    let _home = crate::testutil::HomeSandbox::new();
    plain_live_login("live-refresh");
    let mut app = bare_app();
    app.refresh_unsaved_live_login();
    app.profile_cursor = 0;
    let mut draft = build_draft_new();
    draft.name = InputState::new("fresh");
    draft.captured_login = Some(DraftLogin::Mint(Box::new(login_outcome(
        "stashed",
        Some("uuid-stashed"),
    ))));
    app.config_draft = Some(draft);
    app.config_focus = ConfigFocus::Actions;

    run_config_row(&mut app, ConfigRow::CaptureLogin);

    let action = match app.modals.last() {
        Some(Modal::Confirm(s)) => {
            assert!(
                matches!(s.on_confirm, ConfirmAction::CaptureOverMintStash(_)),
                "the confirm targets the mint replacement"
            );
            s.on_confirm.clone()
        }
        other => panic!("⏎ over a stashed mint must confirm first, got {other:?}"),
    };
    assert!(
        matches!(
            app.config_draft
                .as_ref()
                .and_then(|d| d.captured_login.as_ref()),
            Some(DraftLogin::Mint(_))
        ),
        "cancel (no confirm) keeps the mint"
    );

    run_confirm_action(&mut app, action);

    assert!(
        matches!(
            app.config_draft
                .as_ref()
                .and_then(|d| d.captured_login.as_ref()),
            Some(DraftLogin::LiveLogin(_))
        ),
        "confirming swaps the mint for the captured live login"
    );
}

/// The reverse direction of the same single stash slot: `+ login` over a
/// stashed live login must confirm before replacing it — the gate now guards
/// ANY stash, not just a mint.
#[test]
fn login_row_over_a_stashed_live_login_confirms_first() {
    use super::{
        ConfigFocus, ConfigRow, ConfirmAction, DraftLogin, Modal, build_draft_new, run_config_row,
    };
    use crate::actions::CaptureSnapshot;
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.profile_cursor = 0; // the `+ new` form
    let mut draft = build_draft_new();
    draft.name = InputState::new("fresh");
    draft.captured_login = Some(DraftLogin::LiveLogin(Box::new(CaptureSnapshot {
        credentials: None,
        base_url: None,
        api_key: None,
        anchor: crate::actions::AnchorAction::Unproven,
    })));
    app.config_draft = Some(draft);
    app.config_focus = ConfigFocus::Actions;

    run_config_row(&mut app, ConfigRow::Login);

    assert!(
        matches!(
            app.modals.last(),
            Some(Modal::Confirm(s)) if matches!(s.on_confirm, ConfirmAction::RestartLogin(_, true))
        ),
        "⏎ on `+ login` over a stashed live login must confirm before dropping it",
    );
    assert!(
        app.login.is_none(),
        "no login worker starts until the confirm is accepted",
    );
    assert!(
        matches!(
            app.config_draft
                .as_ref()
                .and_then(|d| d.captured_login.as_ref()),
            Some(DraftLogin::LiveLogin(_))
        ),
        "cancel (no confirm) keeps the stashed live login"
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
        .send((
            1,
            LoginEvent::Stage(LoginStage::ExchangingCode(super::LoginMethod::Browser)),
        ))
        .unwrap();
    // A stale generation's stage bump is ignored.
    app.login_event_tx
        .send((7, LoginEvent::Stage(LoginStage::Verifying)))
        .unwrap();

    drain_login_events(&mut app);

    let session = app.login.as_ref().expect("session stays live");
    assert_eq!(session.url.as_deref(), Some("https://example.test/auth"));
    assert_eq!(
        session.stage,
        LoginStage::ExchangingCode(super::LoginMethod::Browser)
    );
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
    use super::{KeyCode, ServicesFocus, Tab, handle_key};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Services;
    app.services.focus = ServicesFocus::Detail;

    app.services.detail_max_scroll.set(40);
    for _ in 0..60 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
    assert_eq!(app.services.detail_scroll, 40, "↓ stops at the bound");

    app.services.detail_max_scroll.set(12);
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(
        app.services.detail_scroll, 11,
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
        format!(
            "live login is 'play' · not the active 'work' · press {} to resolve",
            crate::tui::render::prose::key("d")
        ),
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

/// The adopt makes its target active OUTSIDE `overwrite_captured_profile`, so
/// it owes the same settings write that fn's own activate arms make. With an
/// endpoint now preserved through a credential-less snapshot, an adopt that
/// skips it leaves the live session routed at Anthropic under a profile whose
/// record says otherwise — the record and `settings.json` must agree.
#[test]
fn adopting_a_divergence_writes_the_targets_endpoint_into_the_live_settings() {
    use super::{ConfirmAction, run_confirm_action};
    use crate::profile::{AppConfig, AppState, Profile, save_profile};
    let _home = crate::testutil::HomeSandbox::new();

    let mut active = Profile::new("work".to_string(), None, None);
    active.credentials = Some(creds_ra("rt-work", "at-work"));
    // The outgoing account carries an `[env]` entry, so the strip this arm
    // performs is actually exercised: without one, removing the strip
    // altogether passes. Not `ANTHROPIC_AUTH_TOKEN`, which every write clears
    // unconditionally.
    active
        .env
        .insert("ANTHROPIC_API_KEY".to_string(), "work-token".to_string());
    let mut target = Profile::new(
        "ds-target".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-live".to_string()),
    );
    target.credentials = Some(creds_ra("rt-target", "at-target"));
    save_profile(&active).expect("save work");
    save_profile(&target).expect("save target");
    write_live_creds(&creds_ra("rt-fresh", "at-fresh"));
    // The outgoing account's env has to be IN the live settings for the strip
    // to have anything to remove; nothing else in this fixture writes it.
    crate::claude::apply_profile_to_claude_settings(&active, &[])
        .expect("seed the outgoing account's env into the live settings");

    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec!["work".into(), "ds-target".into()],
            active_profile: Some("work".into()),
            ..AppState::default()
        },
        profiles: vec![active, target],
    });

    let snapshot = crate::actions::capture_snapshot().expect("capture the live login");
    run_confirm_action(
        &mut app,
        ConfirmAction::AdoptDivergence(Box::new(snapshot), "ds-target".to_string()),
    );

    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some("ds-target"),
        "fixture: the adopt is what activated the target",
    );
    let live = crate::claude::read_claude_endpoint_config().expect("read live endpoint");
    assert_eq!(
        live.base_url.as_deref(),
        Some("https://api.deepseek.com/anthropic"),
        "the newly active profile's preserved endpoint must reach the live settings"
    );
    assert_eq!(live.api_key.as_deref(), Some("sk-live"));

    let settings = crate::profile::claude_dir()
        .ok()
        .map(|d| d.join("settings.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    assert!(
        !settings.contains("work-token"),
        "and the outgoing account's env entry goes with it, rather than sitting \
         in front of the incoming account's endpoint: {settings}"
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

// ── walk order (issue #86): the Config-tab cycle handler ────────────────────

#[test]
fn walk_order_space_cycles_and_persists() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::WalkOrder)
        .unwrap();
    assert_eq!(
        app.config().state.walk_order(),
        crate::profile::WalkOrder::Chain,
        "chain by default"
    );

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config().state.walk_order(),
        crate::profile::WalkOrder::SoonestWeeklyReset,
        "space cycles to soonest weekly reset"
    );

    // Persisted to profiles.toml, not just the in-memory config — reload it
    // fresh, the way a relaunch would pick up the value.
    let reloaded: crate::profile::AppState = toml::from_str(
        &std::fs::read_to_string(crate::profile::clauth_dir().unwrap().join("profiles.toml"))
            .expect("read profiles.toml"),
    )
    .expect("parse profiles.toml");
    assert_eq!(
        reloaded.walk_order(),
        crate::profile::WalkOrder::SoonestWeeklyReset,
        "the cycled value persists to disk"
    );

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config().state.walk_order(),
        crate::profile::WalkOrder::MostWeeklyHeadroom,
        "space cycles on to most weekly headroom"
    );
    let reloaded: crate::profile::AppState = toml::from_str(
        &std::fs::read_to_string(crate::profile::clauth_dir().unwrap().join("profiles.toml"))
            .expect("read profiles.toml"),
    )
    .expect("parse profiles.toml");
    assert_eq!(
        reloaded.walk_order(),
        crate::profile::WalkOrder::MostWeeklyHeadroom,
        "the third value persists to disk too"
    );

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config().state.walk_order(),
        crate::profile::WalkOrder::Chain,
        "space cycles back to chain"
    );
}

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

/// The `auto-start queue` row is inert (dimmed) until some account opts into
/// `auto_start` — a queue with no possible member spaces nothing, so space on
/// it must not arm a control with nothing behind it. One opted-in account
/// makes the same key toggle and persist.
#[test]
fn auto_start_queue_space_noops_until_an_account_opts_in() {
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![Profile::new("kitty".to_string(), None, None)],
    });
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::AutoStartQueue)
        .unwrap();
    assert!(
        !app.config().state.auto_start_queue,
        "the queue defaults off"
    );

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        !app.config().state.auto_start_queue,
        "space no-ops while no account has auto-start on"
    );

    {
        let mut cfg = app.config();
        cfg.find_mut(&"kitty".into()).unwrap().auto_start = true;
    }
    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        app.config().state.auto_start_queue,
        "with an opted-in account space arms the queue"
    );
    let reloaded: AppState = toml::from_str(
        &std::fs::read_to_string(crate::profile::clauth_dir().unwrap().join("profiles.toml"))
            .expect("read profiles.toml"),
    )
    .expect("parse profiles.toml");
    assert!(reloaded.auto_start_queue, "the toggle persists to disk");
}

// ── non-login keys (preserve_non_login_keys) ─────────────────────────────────

/// The row flips the persisted `preserve_non_login_keys` through the real
/// Config router: space off writes `preserve_non_login_keys = false`
/// explicitly, ⏎ back on omits the key again (on is the default), and the
/// reader every relink consults follows the file.
#[test]
fn non_login_keys_row_toggles_and_persists() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::PreserveNonLoginKeys)
        .unwrap();
    assert!(app.config().state.preserve_non_login_keys, "on by default");

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        !app.config().state.preserve_non_login_keys,
        "space toggles it off"
    );
    let path = crate::profile::clauth_dir().unwrap().join("profiles.toml");
    let on_disk = std::fs::read_to_string(&path).expect("read");
    assert!(
        on_disk.contains("preserve_non_login_keys = false"),
        "an explicit off must render or the next load reverts it to on:\n{on_disk}"
    );
    assert!(
        !crate::claude::preserve_non_login_keys(),
        "the reader the relinks consult follows the file"
    );

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    assert!(
        app.config().state.preserve_non_login_keys,
        "⏎ mirrors space"
    );
    let on_disk = std::fs::read_to_string(&path).expect("read");
    assert!(
        !on_disk.contains("preserve_non_login_keys"),
        "on (default) omits the key:\n{on_disk}"
    );
    assert!(crate::claude::preserve_non_login_keys());
}

// ── auto-update (the [update] toggle) ────────────────────────────────────────

/// The row flips the persisted `[update]` table through the real Config
/// router: space off writes `auto_update = false` explicitly, ⏎ back on
/// omits the whole table again (on is the default, so nothing renders).
#[test]
fn auto_update_row_toggles_and_persists() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::AutoUpdate)
        .unwrap();
    assert!(app.config().state.update.auto_update, "on by default");

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        !app.config().state.update.auto_update,
        "space toggles it off"
    );

    let path = crate::profile::clauth_dir().unwrap().join("profiles.toml");
    let reloaded: crate::profile::AppState =
        toml::from_str(&std::fs::read_to_string(&path).expect("read profiles.toml"))
            .expect("parse profiles.toml");
    assert!(!reloaded.update.auto_update, "off persists to disk");
    let on_disk = std::fs::read_to_string(&path).expect("read");
    assert!(
        on_disk.contains("auto_update = false"),
        "an explicit off must render or the next load reverts it to on:\n{on_disk}"
    );

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    assert!(
        app.config().state.update.auto_update,
        "⏎ mirrors space and toggles it back on"
    );
    let on_disk = std::fs::read_to_string(&path).expect("read");
    assert!(
        !on_disk.contains("update"),
        "on (default) omits the whole [update] table:\n{on_disk}"
    );
}

/// `App::new` reads the saved toggle: with `auto_update = false` persisted no
/// update thread spawns — no handle, so no network path is ever driven.
#[test]
fn saved_off_update_leaves_new_app_with_no_update_handle() {
    use crate::profile::AppConfig;
    let _home = crate::testutil::HomeSandbox::new();
    let mut state = crate::profile::AppState::default();
    state.update.auto_update = false;
    crate::profile::save_app_state(&state).expect("persist off toggle");
    let app = App::new(AppConfig {
        state,
        profiles: Vec::new(),
    });
    assert!(
        app.update_handle.is_none(),
        "saved off → no update thread at startup"
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

use super::parse_context_nudge_tokens;
use super::parse_refresh_secs;

/// Park the Config cursor on the refresh-interval row.
fn on_refresh_row(app: &mut App) {
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::RefreshInterval)
        .unwrap();
}

/// Park the Config cursor on the context-nudge row.
fn on_nudge_row(app: &mut App) {
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::ContextNudge)
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

// ── context nudge ───────────────────────────────────────────────────────────

#[test]
fn parse_context_nudge_tokens_accepts_in_range_only() {
    // Raw tokens: a plain number or one trailing `k` (case-insensitive), landing
    // in 50_000..=2_000_000.
    assert_eq!(parse_context_nudge_tokens("600000"), Some(600_000));
    assert_eq!(parse_context_nudge_tokens("600k"), Some(600_000));
    assert_eq!(parse_context_nudge_tokens("600K"), Some(600_000));
    assert_eq!(parse_context_nudge_tokens("50000"), Some(50_000));
    assert_eq!(parse_context_nudge_tokens("2000000"), Some(2_000_000));
    assert!(
        parse_context_nudge_tokens("49999").is_none(),
        "below the 50k floor"
    );
    assert!(
        parse_context_nudge_tokens("2000001").is_none(),
        "above the 2m cap"
    );
    assert!(parse_context_nudge_tokens("1.5m").is_none(), "no m suffix");
    assert!(
        parse_context_nudge_tokens("k").is_none(),
        "bare k has no digits"
    );
    assert!(parse_context_nudge_tokens("").is_none());
    assert!(parse_context_nudge_tokens("abc").is_none());
    assert!(
        parse_context_nudge_tokens(" 600k").is_none(),
        "whitespace invalidates"
    );
    assert!(
        parse_context_nudge_tokens("600 k").is_none(),
        "internal whitespace"
    );
}

#[test]
fn context_nudge_space_cycles_the_full_ladder_wrapping_to_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_nudge_row(&mut app);

    let expect = [
        Some(300_000),
        Some(400_000),
        Some(600_000),
        Some(900_000),
        None,
        Some(300_000),
    ];
    for want in expect {
        super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(
            app.config().state.context_nudge_threshold_tokens(),
            want,
            "space steps off → 300k → 400k → 600k → 900k → off, wrapping"
        );
    }
    assert!(
        app.context_nudge_draft.is_none(),
        "space cycles presets, never opens the editor"
    );
}

#[test]
fn context_nudge_space_from_custom_lands_on_next_preset_or_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_nudge_row(&mut app);
    app.config().state.context_nudge_threshold_tokens = Some(450_000); // between 400k and 600k

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config().state.context_nudge_threshold_tokens(),
        Some(600_000),
        "space from an off-ladder custom value steps to the next preset above it"
    );

    app.config().state.context_nudge_threshold_tokens = Some(1_500_000); // custom past the top preset
    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.config().state.context_nudge_threshold_tokens(),
        None,
        "space from a custom value past the top preset wraps to off"
    );
}

#[test]
fn context_nudge_space_persists_through_profiles_toml() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_nudge_row(&mut app);
    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));

    let reloaded = crate::profile::load_app_state().expect("read profiles.toml");
    assert_eq!(
        reloaded.context_nudge_threshold_tokens,
        Some(300_000),
        "space's step lands on disk the way a relaunch would read it"
    );
}

#[test]
fn context_nudge_enter_opens_editor_seeded() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_nudge_row(&mut app);

    assert!(app.context_nudge_draft.is_none());
    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    let draft = app
        .context_nudge_draft
        .as_ref()
        .expect("⏎ opens the custom-value editor");
    assert_eq!(
        draft.value, "300k",
        "off seeds the first preset, the value one space-press would pick"
    );

    // From a preset the seed is the row's own `k` vocabulary.
    app.context_nudge_draft = None;
    app.config().state.context_nudge_threshold_tokens = Some(600_000);
    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.context_nudge_draft.as_ref().expect("editor open").value,
        "600k",
        "a preset seeds in k form"
    );

    // An exact million seeds in k form too: the parser's grammar takes digits
    // or one trailing k, so an M-form seed would open the editor in DANGER.
    app.context_nudge_draft = None;
    app.config().state.context_nudge_threshold_tokens = Some(2_000_000);
    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.context_nudge_draft.as_ref().expect("editor open").value,
        "2000k",
        "an exact million seeds as typeable k, never M"
    );
}

/// A custom threshold committed, the editor reopened: the seed renders the
/// plain token count and parses — the editor opens out of DANGER.
#[test]
fn context_nudge_reopened_editor_seeds_a_custom_value_as_plain_tokens() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_nudge_row(&mut app);
    app.config().state.context_nudge_threshold_tokens = Some(450_500);

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    let draft = app
        .context_nudge_draft
        .as_ref()
        .expect("⏎ reopens the custom-value editor");
    assert_eq!(
        draft.value, "450500",
        "a custom value seeds as plain tokens"
    );
    assert!(
        parse_context_nudge_tokens(&draft.value).is_some(),
        "the seed is typeable — the editor opens out of DANGER"
    );
}

#[test]
fn context_nudge_custom_value_commits_and_clears() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_nudge_row(&mut app);

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    // Clear the seeded "300k", type "600k".
    for _ in 0..4 {
        super::handle_context_nudge_edit_key(&mut app, key(KeyCode::Backspace));
    }
    for c in "600k".chars() {
        super::handle_context_nudge_edit_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_context_nudge_edit_key(&mut app, key(KeyCode::Enter));

    assert!(
        app.context_nudge_draft.is_none(),
        "a valid commit clears the draft"
    );
    assert_eq!(
        app.config().state.context_nudge_threshold_tokens(),
        Some(600_000)
    );
    let reloaded = crate::profile::load_app_state().expect("read profiles.toml");
    assert_eq!(
        reloaded.context_nudge_threshold_tokens,
        Some(600_000),
        "the commit lands on disk the way a relaunch would read it"
    );
}

#[test]
fn context_nudge_out_of_range_keeps_editor_open() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_nudge_row(&mut app);

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    for _ in 0..4 {
        super::handle_context_nudge_edit_key(&mut app, key(KeyCode::Backspace));
    }
    for c in "49999".chars() {
        super::handle_context_nudge_edit_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_context_nudge_edit_key(&mut app, key(KeyCode::Enter));

    assert!(
        app.context_nudge_draft.is_some(),
        "an out-of-range value keeps the editor open for correction"
    );
    assert_eq!(
        app.config().state.context_nudge_threshold_tokens(),
        None,
        "threshold stays put while the typed value is invalid"
    );
}

#[test]
fn context_nudge_esc_discards_editor() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_nudge_row(&mut app);

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    for c in "900k".chars() {
        super::handle_context_nudge_edit_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_context_nudge_edit_key(&mut app, key(KeyCode::Esc));

    assert!(app.context_nudge_draft.is_none(), "esc discards the editor");
    assert_eq!(
        app.config().state.context_nudge_threshold_tokens(),
        None,
        "esc leaves the threshold unchanged"
    );
}

// ── Setup tab: per-account custom env editor ───────────────────────────────

mod env_editor {
    use super::super::{App, ConfigFocus, ConfigRow, InputState, KeyCode, Modal, Tab, config_rows};
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
    fn expanded_overrides_list_fable_first_then_opus_sonnet_haiku_subagent() {
        let _home = crate::testutil::HomeSandbox::new();
        let mut app = app_with_env(BTreeMap::new());
        enter_detail(&mut app);
        super::super::run_config_row(&mut app, ConfigRow::ModelOverrideAdd);
        let overrides: Vec<ConfigRow> = config_rows(&app)
            .into_iter()
            .filter(|r| {
                matches!(
                    r,
                    ConfigRow::OpusModel
                        | ConfigRow::SonnetModel
                        | ConfigRow::HaikuModel
                        | ConfigRow::FableModel
                        | ConfigRow::SubagentModel
                )
            })
            .collect();
        assert_eq!(
            overrides,
            vec![
                ConfigRow::FableModel,
                ConfigRow::OpusModel,
                ConfigRow::SonnetModel,
                ConfigRow::HaikuModel,
                ConfigRow::SubagentModel,
            ]
        );
    }

    fn app_with_opus_override_editing() -> App {
        let mut profile = Profile::new("acct".to_string(), None, None);
        profile.models.opus = Some("claude-opus-4-8".to_string());
        let mut app = app_with_profile(profile);
        enter_detail(&mut app);
        app.config_action_cursor = config_rows(&app)
            .iter()
            .position(|r| *r == ConfigRow::OpusModel)
            .expect("a set override renders");
        super::super::handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
        assert_eq!(
            app.config_draft.as_ref().and_then(|d| d.active),
            Some(ConfigRow::OpusModel),
            "⏎ opens the opus field"
        );
        for _ in 0.."claude-opus-4-8".len() {
            super::super::handle_key(&mut app, crate::testutil::key(KeyCode::Backspace));
        }
        assert_eq!(
            app.config_draft
                .as_ref()
                .map(|d| d.opus_model.value.as_str()),
            Some(""),
            "every character is gone"
        );
        app
    }

    /// Space on a saved account's `model` row walks default → fable → opus →
    /// sonnet → haiku → opusplan → default, and each step lands on disk.
    #[test]
    fn space_on_a_saved_accounts_model_row_cycles_fable_first_and_persists() {
        let _home = HomeSandbox::new();
        let profile = Profile::new("acct".to_string(), None, None);
        crate::profile::save_app_state(&AppState {
            profiles: vec![profile.name.clone()],
            ..AppState::default()
        })
        .expect("save state");
        crate::profile::save_profile(&profile).expect("save profile");
        let mut app = app_with_profile(profile);
        enter_detail(&mut app);
        app.config_action_cursor = config_rows(&app)
            .iter()
            .position(|r| *r == ConfigRow::Model)
            .expect("a saved account carries the model row");

        for expected in [
            Some("fable"),
            Some("opus"),
            Some("sonnet"),
            Some("haiku"),
            Some("opusplan"),
            None,
        ] {
            super::super::handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
            let on_disk = crate::profile::load_config()
                .expect("reload")
                .profiles
                .into_iter()
                .find(|p| p.name == "acct")
                .expect("the account is on disk")
                .models
                .default;
            assert_eq!(on_disk.as_deref(), expected);
            assert_eq!(
                app.config_draft.as_ref().map(|d| d.model.value.as_str()),
                Some(expected.unwrap_or("")),
                "the row's buffer follows the saved value"
            );
        }
    }

    /// Typing into an OAuth account's empty base url turns it into an api
    /// account mid-edit: `auto-start` above the field drops out and `api key`
    /// joins below it. The cursor stays on the field being typed into, and on
    /// it again once ⎋ reverts the buffer and the rows flip back.
    #[test]
    fn the_cursor_follows_the_base_url_field_while_its_rows_reshape() {
        let _home = crate::testutil::HomeSandbox::new();
        let mut app = app_with_env(BTreeMap::new());
        enter_detail(&mut app);
        let before = config_rows(&app);
        app.config_action_cursor = before
            .iter()
            .position(|r| *r == ConfigRow::BaseUrl)
            .expect("base url row");
        super::super::handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
        super::super::handle_key(&mut app, crate::testutil::key(KeyCode::Char('h')));
        let typed = config_rows(&app);
        assert!(
            !typed.contains(&ConfigRow::AutoStart) && typed.contains(&ConfigRow::ApiKey),
            "precondition: the typed base url flipped the account to api rows"
        );
        assert_eq!(
            typed.get(app.config_action_cursor),
            Some(&ConfigRow::BaseUrl)
        );
        super::super::handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
        assert_eq!(config_rows(&app), before, "⎋ restores the oauth rows");
        assert_eq!(
            before.get(app.config_action_cursor),
            Some(&ConfigRow::BaseUrl)
        );
    }

    #[test]
    fn an_override_emptied_mid_edit_keeps_its_row_and_the_cursor() {
        let _home = crate::testutil::HomeSandbox::new();
        let app = app_with_opus_override_editing();
        let rows = config_rows(&app);
        assert_eq!(
            rows.get(app.config_action_cursor),
            Some(&ConfigRow::OpusModel),
            "the emptied field stays under the cursor"
        );
    }

    #[test]
    fn an_override_committed_empty_stays_until_the_detail_pane_closes() {
        let _home = crate::testutil::HomeSandbox::new();
        let mut app = app_with_opus_override_editing();
        super::super::handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
        assert_eq!(
            app.config()
                .find(&crate::profile::ProfileName::from("acct"))
                .and_then(|p| p.models.opus.clone()),
            None,
            "an emptied override unsets the alias"
        );
        assert_eq!(
            config_rows(&app).get(app.config_action_cursor),
            Some(&ConfigRow::OpusModel),
            "the emptied field stays where it was"
        );
        super::super::leave_config_detail(&mut app);
        enter_detail(&mut app);
        let rows = config_rows(&app);
        assert!(
            !rows.contains(&ConfigRow::OpusModel) && rows.contains(&ConfigRow::ModelOverrideAdd),
            "a fresh draft tucks the now-unset override behind the chip"
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
    fn inherited_settings_collision_writes_only_after_explicit_confirmation() {
        let _home = HomeSandbox::new();
        let key = "ENDPOINT";
        let name = crate::profile::ProfileName::from("acct");
        let profile = Profile::new("acct".to_string(), None, None);
        crate::profile::save_profile(&profile).expect("seed account");
        let settings_path = crate::profile::claude_dir()
            .expect("claude dir")
            .join("settings.json");
        crate::profile::atomic_write(&settings_path, "{\"env\":{\"ENDPOINT\":\"inherited\"}}")
            .expect("seed inherited env");
        let mut app = App::new(AppConfig {
            state: AppState::default(),
            profiles: vec![profile],
        });
        enter_detail(&mut app);
        if let Some(draft) = app.config_draft.as_mut() {
            draft.env_new_key = InputState::new(key);
            draft.active = Some(ConfigRow::EnvAdd);
        }
        let profile_path =
            crate::profile::profile_subpath(&name, "config.toml").expect("profile path");
        let before = std::fs::read(&profile_path).expect("read seeded profile");
        assert_eq!(app.config().find(&name).and_then(|p| p.env.get(key)), None);
        super::super::commit_env_new_key(&mut app);
        assert!(
            matches!(app.modals.last(), Some(Modal::EnvCollision(form)) if form.reason == "your ~/.claude/settings.json")
        );
        assert_eq!(app.config().find(&name).and_then(|p| p.env.get(key)), None);
        assert_eq!(
            std::fs::read(&settings_path).expect("read settings"),
            b"{\"env\":{\"ENDPOINT\":\"inherited\"}}"
        );
        assert_eq!(std::fs::read(&profile_path).expect("read profile"), before);

        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Up));
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Up));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 14)).expect("backend");
        terminal
            .draw(|frame| crate::tui::render::draw(frame, &app))
            .expect("render choice");
        assert!(
            matches!(app.modals.last(), Some(Modal::EnvCollision(form)) if form.overwrite_needs_review.get())
        );
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Enter));
        assert!(
            matches!(app.modals.last(), Some(Modal::EnvCollision(form)) if form.reviewing_overwrite && !form.confirm_overwrite)
        );
        assert_eq!(std::fs::read(&profile_path).expect("read profile"), before);
        assert_eq!(
            std::fs::read(&settings_path).expect("read settings"),
            b"{\"env\":{\"ENDPOINT\":\"inherited\"}}"
        );
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Enter));
        assert!(
            matches!(app.modals.last(), Some(Modal::EnvCollision(form)) if !form.reviewing_overwrite)
        );
        assert_eq!(std::fs::read(&profile_path).expect("read profile"), before);
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Enter));
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Esc));
        assert!(
            matches!(app.modals.last(), Some(Modal::EnvCollision(form)) if !form.reviewing_overwrite)
        );
        assert_eq!(std::fs::read(&profile_path).expect("read profile"), before);
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Enter));
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Right));
        assert_eq!(std::fs::read(&profile_path).expect("read profile"), before);
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Enter));
        assert!(
            app.modals.is_empty(),
            "explicit affirmative closes the prompt"
        );
        assert_eq!(
            app.config()
                .find(&name)
                .and_then(|p| p.env.get(key))
                .map(String::as_str),
            Some("")
        );
        assert_ne!(std::fs::read(&profile_path).expect("read profile"), before);
        assert_eq!(
            std::fs::read(&settings_path).expect("read settings"),
            b"{\"env\":{\"ENDPOINT\":\"inherited\"}}"
        );
    }

    #[test]
    fn fitting_managed_collision_adds_without_a_second_confirmation() {
        let _home = HomeSandbox::new();
        let mut app = app_with_env(BTreeMap::new());
        enter_detail(&mut app);
        let key = "ANTHROPIC_BASE_URL";
        if let Some(draft) = app.config_draft.as_mut() {
            draft.env_new_key = InputState::new(key);
            draft.active = Some(ConfigRow::EnvAdd);
        }
        super::super::commit_env_new_key(&mut app);
        assert!(matches!(app.modals.last(), Some(Modal::EnvCollision(_))));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).expect("backend");
        terminal
            .draw(|frame| crate::tui::render::draw(frame, &app))
            .expect("render fitting choice");
        assert!(
            matches!(app.modals.last(), Some(Modal::EnvCollision(form)) if !form.overwrite_needs_review.get())
        );
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Up));
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Up));
        assert_eq!(
            app.config()
                .find(&crate::profile::ProfileName::from("acct"))
                .and_then(|p| p.env.get(key).cloned()),
            None
        );
        super::super::handle_modal_key(&mut app, crate::testutil::key(KeyCode::Enter));
        assert!(app.modals.is_empty());
        assert_eq!(
            app.config()
                .find(&crate::profile::ProfileName::from("acct"))
                .and_then(|p| p.env.get(key).cloned()),
            Some(String::new())
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

    /// Emptying a custom env entry's value and committing is an unset, not a
    /// blank: the key must disappear from the profile's config.toml AND from
    /// the live settings.json (owner ruling 2026-09-02).
    #[test]
    fn committing_an_emptied_env_value_removes_the_key_everywhere() {
        let _home = HomeSandbox::new();
        let name = crate::profile::ProfileName::from("acct");
        let mut env = BTreeMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let mut profile = Profile::new("acct".to_string(), None, None);
        profile.env = env;
        crate::profile::save_profile(&profile).expect("seed the profile on disk");

        // The owner-reported pre-state: the key is ALREADY in the live settings
        // with its old value, so the settings assert below exercises the
        // prev-key strip, not an empty fixture (an unseeded assert stays green
        // under a strip break — mutation M1a, 2026-09-02 review).
        let claude_dir = crate::profile::claude_dir().expect("claude dir");
        crate::profile::atomic_write(
            &claude_dir.join("settings.json"),
            "{\"env\":{\"FOO\":\"bar\"}}",
        )
        .expect("seed the live settings with the pre-existing key");

        // Active marker on: the commit must also re-apply the live settings.
        let state = AppState {
            active_profile: Some(name.clone()),
            ..AppState::default()
        };
        let mut app = App::new(AppConfig {
            state,
            profiles: vec![profile],
        });
        enter_detail(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.env_value = InputState::new("");
            d.active = Some(ConfigRow::EnvEntry(0));
        }
        super::super::commit_env_value(&mut app, 0);

        assert_eq!(
            app.config()
                .find(&name)
                .and_then(|p| p.env.get("FOO").cloned()),
            None,
            "the emptied entry is gone from the in-memory profile"
        );

        let cfg_path =
            crate::profile::profile_subpath(&name, "config.toml").expect("config.toml path");
        let cfg_text = std::fs::read_to_string(&cfg_path).expect("read config.toml");
        assert!(
            !cfg_text.contains("FOO"),
            "no key and no empty value survive in config.toml: {cfg_text}"
        );

        let settings_path = crate::profile::claude_dir()
            .expect("claude dir")
            .join("settings.json");
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("read settings.json"),
        )
        .expect("settings.json parses");
        let env_block = settings
            .get("env")
            .and_then(serde_json::Value::as_object)
            .expect("settings.json has an env object");
        assert!(
            !env_block.contains_key("FOO"),
            "no key and no empty value survive in settings.json: {settings}"
        );
    }

    /// Escaping a just-added env entry removes the row (cancel semantics,
    /// owner ruling 2026-09-02): the add only persisted an empty placeholder,
    /// so backing out restores the pre-add state everywhere.
    #[test]
    fn escaping_a_just_added_env_entry_removes_the_row_everywhere() {
        let _home = HomeSandbox::new();
        let name = crate::profile::ProfileName::from("acct");
        let profile = Profile::new("acct".to_string(), None, None);
        crate::profile::save_profile(&profile).expect("seed the profile on disk");
        let state = AppState {
            active_profile: Some(name.clone()),
            ..AppState::default()
        };
        let mut app = App::new(AppConfig {
            state,
            profiles: vec![profile],
        });
        enter_detail(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.env_new_key = InputState::new("FOO");
            d.active = Some(ConfigRow::EnvAdd);
        }
        super::super::commit_env_new_key(&mut app);

        // The add persists the empty placeholder to both files — the seed the
        // escape's strip must actually remove, not an empty fixture.
        assert_eq!(
            app.config()
                .find(&name)
                .and_then(|p| p.env.get("FOO").cloned()),
            Some(String::new()),
            "the add persists the empty placeholder for the value editor"
        );
        let cfg_path =
            crate::profile::profile_subpath(&name, "config.toml").expect("config.toml path");
        assert!(
            std::fs::read_to_string(&cfg_path)
                .expect("read config.toml")
                .contains("FOO"),
            "the placeholder is on disk before the escape"
        );
        let settings_path = crate::profile::claude_dir()
            .expect("claude dir")
            .join("settings.json");
        let env_block = || {
            let settings: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&settings_path).expect("read settings.json"),
            )
            .expect("settings.json parses");
            settings
                .get("env")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .expect("settings.json has an env object")
        };
        assert!(
            env_block().contains_key("FOO"),
            "the live settings hold the key before the escape"
        );

        super::super::cancel_config_edit(&mut app, ConfigRow::EnvEntry(0));

        assert_eq!(
            app.config()
                .find(&name)
                .and_then(|p| p.env.get("FOO").cloned()),
            None,
            "the just-added entry is gone from the in-memory profile"
        );
        assert!(
            app.config_draft.as_ref().and_then(|d| d.active).is_none(),
            "the escape ends the value edit"
        );
        let cfg_text = std::fs::read_to_string(&cfg_path).expect("read config.toml");
        assert!(
            !cfg_text.contains("FOO"),
            "no key and no empty value survive in config.toml: {cfg_text}"
        );
        assert!(
            !env_block().contains_key("FOO"),
            "no key and no empty value survive in settings.json: {:?}",
            env_block()
        );
    }

    /// Escape on an EXISTING entry discards the edit only: the saved value
    /// survives in memory, on disk, and in the live settings, even when the
    /// operator had emptied the buffer — the remove must key on the just-added
    /// state, never on an empty buffer.
    #[test]
    fn escape_on_an_existing_env_entry_keeps_the_saved_value() {
        let _home = HomeSandbox::new();
        let name = crate::profile::ProfileName::from("acct");
        let mut env = BTreeMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let mut profile = Profile::new("acct".to_string(), None, None);
        profile.env = env;
        crate::profile::save_profile(&profile).expect("seed the profile on disk");
        crate::claude::apply_profile_to_claude_settings(&profile, &[])
            .expect("seed the live settings");
        let state = AppState {
            active_profile: Some(name.clone()),
            ..AppState::default()
        };
        let mut app = App::new(AppConfig {
            state,
            profiles: vec![profile],
        });
        enter_detail(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            // The operator emptied the buffer — escape must discard that edit.
            d.env_value = InputState::new("");
            d.active = Some(ConfigRow::EnvEntry(0));
        }
        super::super::cancel_config_edit(&mut app, ConfigRow::EnvEntry(0));

        assert_eq!(
            app.config()
                .find(&name)
                .and_then(|p| p.env.get("FOO").cloned()),
            Some("bar".to_string()),
            "the existing entry keeps its saved value in memory"
        );
        assert_eq!(
            app.config_draft
                .as_ref()
                .map(|d| d.env_value.trimmed().to_string()),
            Some("bar".to_string()),
            "the buffer reseeds from the saved value"
        );
        let cfg_path =
            crate::profile::profile_subpath(&name, "config.toml").expect("config.toml path");
        let cfg_text = std::fs::read_to_string(&cfg_path).expect("read config.toml");
        assert!(
            cfg_text.contains("FOO"),
            "the existing entry stays in config.toml: {cfg_text}"
        );
        let settings_path = crate::profile::claude_dir()
            .expect("claude dir")
            .join("settings.json");
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("read settings.json"),
        )
        .expect("settings.json parses");
        let env_block = settings
            .get("env")
            .and_then(serde_json::Value::as_object)
            .expect("settings.json has an env object");
        assert_eq!(
            env_block.get("FOO").and_then(serde_json::Value::as_str),
            Some("bar"),
            "the existing entry keeps its value in the live settings: {settings}"
        );
    }

    /// The just-added cancel arms only until the entry's own value commit:
    /// after committing a value, an escape is the plain edit-cancel and must
    /// keep the committed value (the disarm in `commit_env_value`).
    #[test]
    fn escape_after_a_value_commit_keeps_the_committed_value() {
        let _home = HomeSandbox::new();
        let name = crate::profile::ProfileName::from("acct");
        let profile = Profile::new("acct".to_string(), None, None);
        crate::profile::save_profile(&profile).expect("seed the profile on disk");
        let state = AppState {
            active_profile: Some(name.clone()),
            ..AppState::default()
        };
        let mut app = App::new(AppConfig {
            state,
            profiles: vec![profile],
        });
        enter_detail(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.env_new_key = InputState::new("FOO");
            d.active = Some(ConfigRow::EnvAdd);
        }
        super::super::commit_env_new_key(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.env_value = InputState::new("qux");
        }
        super::super::commit_env_value(&mut app, 0);

        // Re-open the entry's editor and escape: the commit disarmed the
        // just-added cancel, so the generic cancel runs and keeps "qux".
        super::super::run_config_row(&mut app, ConfigRow::EnvEntry(0));
        super::super::cancel_config_edit(&mut app, ConfigRow::EnvEntry(0));

        assert_eq!(
            app.config()
                .find(&name)
                .and_then(|p| p.env.get("FOO").cloned()),
            Some("qux".to_string()),
            "the committed value survives the escape in memory"
        );
        assert_eq!(
            app.config_draft
                .as_ref()
                .map(|d| d.env_value.trimmed().to_string()),
            Some("qux".to_string()),
            "the buffer reseeds from the committed value"
        );
        let cfg_path =
            crate::profile::profile_subpath(&name, "config.toml").expect("config.toml path");
        let cfg_text = std::fs::read_to_string(&cfg_path).expect("read config.toml");
        assert!(
            cfg_text.contains("FOO") && cfg_text.contains("qux"),
            "the committed entry stays in config.toml: {cfg_text}"
        );
        let settings_path = crate::profile::claude_dir()
            .expect("claude dir")
            .join("settings.json");
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).expect("read settings.json"),
        )
        .expect("settings.json parses");
        let env_block = settings
            .get("env")
            .and_then(serde_json::Value::as_object)
            .expect("settings.json has an env object");
        assert_eq!(
            env_block.get("FOO").and_then(serde_json::Value::as_str),
            Some("qux"),
            "the committed entry keeps its value in the live settings: {settings}"
        );
    }

    /// A failed cancel-persist restores the placeholder into memory (the Err
    /// arm of `cancel_just_added_env`): `edit_profile_env` mutates the profile
    /// before its disk writes, so without the restore memory would drop the
    /// key the disk still holds and the retry's index lookup would miss the
    /// row it cancelled.
    #[test]
    fn a_failed_cancel_persist_restores_the_placeholder_for_retry() {
        let _home = HomeSandbox::new();
        let name = crate::profile::ProfileName::from("acct");
        let profile = Profile::new("acct".to_string(), None, None);
        crate::profile::save_profile(&profile).expect("seed the profile on disk");
        let state = AppState {
            active_profile: Some(name.clone()),
            ..AppState::default()
        };
        let mut app = App::new(AppConfig {
            state,
            profiles: vec![profile],
        });
        enter_detail(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.env_new_key = InputState::new("FOO");
            d.active = Some(ConfigRow::EnvAdd);
        }
        super::super::commit_env_new_key(&mut app);

        // Block the config.toml write (a directory at its path makes the
        // atomic rename fail), so the cancel's persist errors.
        let cfg_path =
            crate::profile::profile_subpath(&name, "config.toml").expect("config.toml path");
        std::fs::remove_file(&cfg_path).expect("drop the seed config.toml");
        std::fs::create_dir(&cfg_path).expect("block the config.toml path with a directory");

        super::super::cancel_config_edit(&mut app, ConfigRow::EnvEntry(0));

        assert_eq!(
            app.config()
                .find(&name)
                .and_then(|p| p.env.get("FOO").cloned()),
            Some(String::new()),
            "a failed persist restores the placeholder in memory"
        );
        assert_eq!(
            app.config_draft
                .as_ref()
                .and_then(|d| d.env_just_added.as_deref()),
            Some("FOO"),
            "the cancel stays armed for the retry"
        );

        // Lift the block; the retry finds the same row and removes it.
        std::fs::remove_dir(&cfg_path).expect("lift the block");
        super::super::cancel_config_edit(&mut app, ConfigRow::EnvEntry(0));
        assert_eq!(
            app.config()
                .find(&name)
                .and_then(|p| p.env.get("FOO").cloned()),
            None,
            "the retry removes the just-added entry"
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
            ..crate::profile::OAuthToken::default_extra()
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
    // The chain-edit actions confirm the roster and the chain off disk before
    // persisting, so the fixture's state is on disk as well as in memory.
    let registered: Vec<&str> = names.iter().map(|n| n.as_str()).collect();
    crate::testutil::register_names(&registered);
    let mut on_disk = crate::profile::load_app_state().expect("load the fixture state");
    on_disk.fallback_chain = names.clone();
    crate::profile::save_app_state(&on_disk).expect("persist the fixture chain");
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

#[cfg(unix)]
#[test]
fn reorder_chain_member_keeps_the_cursor_when_the_save_fails() {
    use std::os::unix::fs::PermissionsExt;

    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("a")),
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("b")),
    ]);
    app.chain_cursor = 0;

    // Fail the whole-state save the reorder leg performs; `set_chain_order`
    // re-reads fresh state and saves into ~/.clauth, which 0o500 refuses.
    let restore = crate::profile::clauth_dir().expect("clauth dir");
    std::fs::set_permissions(&restore, std::fs::Permissions::from_mode(0o500))
        .expect("chmod clauth dir read-only");

    super::reorder_chain_member(&mut app, 1);

    std::fs::set_permissions(&restore, std::fs::Permissions::from_mode(0o700))
        .expect("restore clauth dir perms");

    assert_eq!(
        app.chain_cursor, 0,
        "the cursor stays on the member when the reorder never landed"
    );
}

#[test]
fn write_threshold_silently_noops_for_a_vanished_member() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("a")),
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("b")),
    ]);
    app.chain_cursor = 0;

    // Drop "a" from disk after the in-memory app was built: the daemon/TUI
    // reload window, posed for the threshold leg.
    let a = crate::profile::ProfileName::from("a");
    let mut state = crate::profile::load_app_state().expect("read before");
    state.profiles.retain(|n| n.as_str() != a.as_str());
    state.fallback_chain.retain(|n| n.as_str() != a.as_str());
    crate::profile::save_app_state(&state).expect("drop a from disk");

    super::write_threshold(&mut app, &a, 90.0);

    assert!(
        app.toasts.is_empty(),
        "a vanished member is the baseline's silent no-op, not a wire-code toast"
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

    let _home = crate::testutil::HomeSandbox::new();
    let mut a = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    a.last_resort = true;
    let mut app = app_with_unlinked_profiles(vec![
        a,
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("b")),
    ]);
    app.chain_cursor = 0;

    // Block the very first write: `save_profile` does `mkdir_700` under
    // `~/.clauth/profiles`, which fails once `~/.clauth` (created by the
    // fixture's roster save) refuses new children.
    let restore = crate::profile::clauth_dir().expect("clauth dir");
    std::fs::set_permissions(&restore, std::fs::Permissions::from_mode(0o500))
        .expect("chmod clauth dir read-only");

    super::toggle_preferred(&mut app);

    // Restore before any assertion so a failure still lets the sandbox clean up.
    std::fs::set_permissions(&restore, std::fs::Permissions::from_mode(0o700))
        .expect("restore clauth dir perms");

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
        card_edit_row(&app) == Some(super::FallbackRow::WeeklyAt),
        "space still opens the weekly-at editor"
    );
    super::handle_key(&mut app, key(KeyCode::Esc));

    app.fallback_detail_cursor = max_spend_row();
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        card_edit_row(&app) == Some(super::FallbackRow::MaxSpend),
        "space still opens the max-spend editor"
    );
}

// ── fallback max auto-spend (real money) ────────────────────────────────────

/// The row the member card's open edit sits on, `None` at rest.
fn card_edit_row(app: &App) -> Option<super::FallbackRow> {
    app.fallback_edit.as_ref().map(|e| e.state.row())
}

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
    assert!(
        card_edit_row(&app) == Some(super::FallbackRow::MaxSpend),
        "⏎ opens the field"
    );

    // The field opens seeded with the current ceiling ("0.00"), so clear it
    // before typing or the digits append to it.
    for _ in 0..4 {
        super::handle_key(&mut app, key(KeyCode::Backspace));
    }
    for c in ['2', '5'] {
        super::handle_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_key(&mut app, key(KeyCode::Enter));
    assert!(app.fallback_edit.is_none(), "⏎ closes the field");
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
        app.fallback_edit.is_none(),
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
        card_edit_row(&app) == Some(super::FallbackRow::MaxSpend),
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
            ..crate::profile::OAuthToken::default_extra()
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

#[test]
fn usage_roster_reload_with_no_accounts_returns_to_selector() {
    use super::{StatusFocus, Tab, handle_key, on_tick};
    use ratatui::crossterm::event::KeyCode;

    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("alpha"),
    )]);
    app.tab = Tab::Usage;
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.usage_detail_focus, StatusFocus::Detail);
    app.usage_detail_scroll = 7;
    let mut state = crate::profile::load_app_state().expect("read roster before external removal");
    state.profiles.clear();
    state.fallback_chain.clear();
    crate::profile::save_app_state(&state).expect("external removal saves roster");
    let path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("profiles.toml");
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000),
    );
    on_tick(&mut app);
    assert_eq!(
        app.profile_count(),
        0,
        "the reload removed the last account"
    );
    assert_eq!(app.tab, Tab::Usage, "the tab remains Usage");
    assert_eq!(
        app.usage_detail_focus,
        StatusFocus::List,
        "empty roster returns to selector"
    );
    assert_eq!(
        app.usage_detail_scroll, 0,
        "the old detail scroll does not survive"
    );
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 14)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        rows.iter().any(|row| row.contains("no accounts yet")),
        "empty Usage selector is visible"
    );
    assert!(
        rows[13].contains("q quit"),
        "footer no longer advertises descended focus"
    );
}

#[test]
fn usage_read_only_detail_keys_keep_account_and_return_to_selector() {
    use super::{StatusFocus, Tab, handle_key};
    use crate::profile::{AppConfig, AppState, ProfileName};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let first = crate::testutil::blank_profile(&ProfileName::from("alpha"));
    let second = crate::testutil::blank_profile(&ProfileName::from("beta"));
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![first.name.clone(), second.name.clone()],
            ..AppState::default()
        },
        profiles: vec![first, second],
    });
    app.tab = Tab::Usage;
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(app.profile_cursor, 1, "at rest ↓ changes account");
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.usage_detail_focus, StatusFocus::Detail);
    app.usage_detail_max_scroll.set(20);
    app.usage_detail_viewport.set(8);
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(
        app.usage_detail_scroll, 8,
        "page moves by the current viewport"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(app.usage_detail_scroll, 9, "↓ scrolls one real detail line");
    assert_eq!(
        app.profile_cursor, 1,
        "detail scroll does not change account"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Left));
    assert_eq!(app.usage_detail_focus, StatusFocus::List);
    assert_eq!(app.tab, Tab::Usage, "← ascends rather than switching tabs");
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(app.profile_cursor, 0, "↑ once again selects accounts");
}

#[test]
fn tokens_models_detail_keys_keep_model_and_return_to_selector() {
    use super::{StatusFocus, Tab, TokenView, handle_key};
    use crate::tokens::{ModelTokens, TokenStats};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Tokens;
    app.token_view = TokenView::Models;
    app.token_stats = Some(TokenStats {
        models: vec![
            ModelTokens {
                model: "claude-alpha".into(),
                input: 10_000_000,
                ..Default::default()
            },
            ModelTokens {
                model: "claude-beta".into(),
                input: 5_000_000,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(app.token_model_cursor, 1, "at rest ↓ changes model");
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.model_detail_focus, StatusFocus::Detail);
    app.model_detail_max_scroll.set(15);
    app.model_detail_viewport.set(6);
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(app.model_detail_scroll, 6, "page moves by current viewport");
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(app.model_detail_scroll, 7);
    assert_eq!(app.token_model_cursor, 1, "detail scroll retains model");
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert_eq!(app.model_detail_focus, StatusFocus::List);
    assert_eq!(
        app.token_view,
        TokenView::Models,
        "esc ascends only one level"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(app.token_model_cursor, 0, "↑ once again selects models");
}

#[test]
fn models_detail_resets_scroll_when_period_or_filter_changes() {
    use super::{Tab, TokenFilter, TokenPeriod, TokenView, set_token_filter, set_token_period};
    use crate::tokens::{ModelTokens, TokenStats};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Tokens;
    app.token_view = TokenView::Models;
    app.token_stats = Some(TokenStats {
        models: vec![
            ModelTokens {
                model: "claude-alpha".into(),
                input: 10_000_000,
                ..Default::default()
            },
            ModelTokens {
                model: "other-beta".into(),
                input: 5_000_000,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    app.model_detail_scroll = 9;
    set_token_filter(&mut app, TokenFilter::Others);
    assert_eq!(
        app.model_detail_scroll, 0,
        "a changed filtered model starts at the top"
    );
    app.model_detail_scroll = 8;
    set_token_period(&mut app, TokenPeriod::Daily);
    assert_eq!(
        app.model_detail_scroll, 0,
        "a new period starts at its first detail row"
    );
}

#[test]
fn empty_model_list_returns_detail_focus_on_filter_period_and_load() {
    use super::{
        ActionMenuAction, StatusFocus, Tab, TokenFilter, TokenPeriod, TokenView,
        dispatch_action_menu_action, drain_tokens_events, handle_key,
    };
    use crate::tokens::{ModelTokens, TokenStats, TokensEvent};
    use ratatui::crossterm::event::KeyCode;

    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Tokens;
    app.token_stats = Some(TokenStats {
        models: vec![ModelTokens {
            model: "claude-opus-4-8".into(),
            input: 10_000_000,
            ..Default::default()
        }],
        ..Default::default()
    });
    let (tx, rx) = std::sync::mpsc::channel();
    app.tokens_events = rx;

    for producer in ["filter", "period key", "period menu", "loaded"] {
        app.token_view = TokenView::Dashboard;
        app.token_filter = TokenFilter::All;
        app.token_period = TokenPeriod::Lifetime;
        app.model_detail_focus = StatusFocus::List;
        handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
        assert_eq!(
            app.token_view,
            TokenView::Models,
            "{producer}: model rows are enterable"
        );
        handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
        assert_eq!(
            app.model_detail_focus,
            StatusFocus::Detail,
            "{producer}: detail is focused"
        );
        app.model_detail_scroll = 7;
        match producer {
            "filter" => dispatch_action_menu_action(&mut app, ActionMenuAction::TokensShowOthers),
            "period key" => handle_key(&mut app, crate::testutil::key(KeyCode::Char('t'))),
            "period menu" => {
                dispatch_action_menu_action(&mut app, ActionMenuAction::TokensPeriodWeekly)
            }
            "loaded" => {
                tx.send(TokensEvent::Loaded(Box::<TokenStats>::default()))
                    .unwrap();
                drain_tokens_events(&mut app);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            super::token_model_count(&app),
            0,
            "{producer}: no model remains"
        );
        assert_eq!(
            app.token_view,
            TokenView::Models,
            "{producer}: keep the models screen"
        );
        assert_eq!(
            app.model_detail_focus,
            StatusFocus::List,
            "{producer}: return to selector"
        );
        assert_eq!(
            app.model_detail_scroll, 0,
            "{producer}: reset the missing detail's scroll"
        );
        app.token_stats = Some(TokenStats {
            models: vec![ModelTokens {
                model: "claude-opus-4-8".into(),
                input: 10_000_000,
                ..Default::default()
            }],
            ..Default::default()
        });
    }
}

#[test]
fn calendar_rollover_reconciles_empty_model_detail_on_tick() {
    use super::{StatusFocus, Tab, TokenPeriod, TokenView, on_tick};
    use crate::tokens::{DayModelTokens, ModelTokens, TokenStats};

    let _home = crate::testutil::HomeSandbox::new();
    let prior = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs() - 40 * 24 * 60 * 60);
    let prior = prior[..10].to_owned();
    for period in [TokenPeriod::Weekly, TokenPeriod::Monthly] {
        let mut app = bare_app();
        app.tab = Tab::Tokens;
        app.token_view = TokenView::Models;
        app.token_period = period;
        app.token_stats = Some(TokenStats {
            daily_models: vec![DayModelTokens {
                date: prior.clone(),
                model: "claude-opus-4-8".into(),
                in_out: 20,
                split: Some(ModelTokens {
                    model: "claude-opus-4-8".into(),
                    input: 12,
                    output: 8,
                    ..Default::default()
                }),
                hours: None,
            }],
            ..Default::default()
        });
        let bucket = period.bucket().expect("scoped lens");
        let (from, to) = crate::tokens::current_bucket_bounds(&prior, bucket);
        assert_eq!(
            crate::tokens::period_models(
                &app.token_stats.as_ref().expect("stats").daily_models,
                &from,
                &to
            )
            .len(),
            1,
            "this model was selectable in its previous calendar bucket"
        );
        app.model_detail_focus = StatusFocus::Detail;
        app.model_detail_scroll = 7;
        assert_eq!(
            super::token_model_count(&app),
            0,
            "the new calendar bucket has no models"
        );
        on_tick(&mut app);
        assert_eq!(app.token_view, TokenView::Models);
        assert_eq!(
            app.model_detail_focus,
            StatusFocus::List,
            "{period:?}: empty detail ascends at the next tick"
        );
        assert_eq!(
            app.model_detail_scroll, 0,
            "{period:?}: stale detail scroll clears"
        );
    }
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
            "reload stats",
            "start daemon",
            "start shunt",
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

/// A plain live credentials file, as `claude` itself leaves it, holding an
/// OAuth login (refresh token `rt-<name>` when `name` is given, so a profile
/// saved with the same token owns it).
fn plain_live_login(refresh: &str) -> std::path::PathBuf {
    let live = crate::profile::claude_dir()
        .expect("claude dir")
        .join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(
        &live,
        serde_json::to_vec(&crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: format!("access-{refresh}"),
                refresh_token: Some(refresh.to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
                ..crate::profile::OAuthToken::default_extra()
            }),
        })
        .expect("serialize live login"),
    )
    .expect("write live login");
    live
}

/// `+ capture current login` renders on the `+ new` form only while the live
/// login is real and no profile owns it — `unsaved_live_login` drives the row,
/// and the flag itself reads live credentials + the profile set. API mode
/// keeps the row (`+ login` doesn't): a live setup can be an endpoint too.
#[test]
fn new_form_capture_row_tracks_an_unsaved_live_login() {
    use super::{ConfigRow, build_draft_new, config_rows};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.profile_cursor = 0; // == profile_count() → the `+ new` form

    // Nothing live: no row (the flag starts false — no login exists).
    assert!(
        !config_rows(&app).contains(&ConfigRow::CaptureLogin),
        "no live login, no capture row"
    );

    // An unowned live login: the row shows, and survives a typed base url.
    plain_live_login("live-refresh");
    app.refresh_unsaved_live_login();
    assert!(
        app.unsaved_live_login,
        "an unowned live login turns the flag on"
    );
    assert!(
        config_rows(&app).contains(&ConfigRow::CaptureLogin),
        "the row renders for an unowned live login"
    );
    let mut draft = build_draft_new();
    draft.base_url = InputState::new("https://api.example.com");
    app.config_draft = Some(draft);
    assert!(
        config_rows(&app).contains(&ConfigRow::CaptureLogin),
        "the row survives api mode (a live setup can be an endpoint)"
    );
    app.config_draft = None;

    // A profile owning the login: the flag drops, the row goes.
    plain_live_login("rt-owner");
    let owner = stored_oauth_profile("owner", far_future());
    {
        let mut cfg = app.config();
        cfg.profiles.push(owner);
    }
    app.refresh_unsaved_live_login();
    assert!(
        !app.unsaved_live_login,
        "a login a profile owns turns the flag off"
    );
    assert!(
        !config_rows(&app).contains(&ConfigRow::CaptureLogin),
        "no row over an owned live login"
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
        anchor: crate::actions::AnchorAction::Unproven,
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
        anchor: crate::actions::AnchorAction::Unproven,
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

        for expected in ["fable", "opus", "sonnet", "haiku", "opusplan"] {
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
        cycle_model(&mut app); // "" -> "fable"

        commit_new_account(&mut app);

        assert_eq!(
            app.config()
                .find(&crate::profile::ProfileName::from("fresh"))
                .and_then(|p| p.models.default.clone()),
            Some("fable".to_string()),
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
                shape: Default::default(),
            },
            crate::tokens::ModelTokens {
                model: "claude-sonnet-4".into(),
                input: 8,
                output: 4,
                cache_read: 0,
                cache_create: 0,
                shape: Default::default(),
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
// would be a second one racing it. The TUI re-reads the file when its content
// fingerprint (byte length + tail hash) changes, which is also covered below.
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

/// Persist the gate profile and give it an on-disk OAuth cache body at
/// `GATE_UTIL` fetched `age_ms` ago, so `apply_usage` grades its age.
fn seed_gate_cache(age_ms: u64) {
    let name = crate::profile::ProfileName::from(GATE_PROFILE);
    let mut profile = crate::testutil::blank_profile(&name);
    profile.bell_threshold = Some(GATE_THRESHOLD);
    crate::profile::save_profile(&profile).expect("persist gate profile");
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec![name.clone()],
        ..crate::profile::AppState::default()
    })
    .expect("persist gate state");
    let body = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: GATE_UTIL,
            resets_at: None,
        }),
        fetched_at: Some(crate::usage::now_ms().saturating_sub(age_ms)),
        ..UsageInfo::default()
    };
    crate::profile_cache::write_profile_cache(&name, crate::profile_cache::USAGE_CACHE_FILE, &body);
}

#[test]
fn apply_usage_skips_the_bell_on_an_aged_reading_under_a_fresh_status() {
    let _home = crate::testutil::HomeSandbox::new();
    seed_gate_cache(3_600_000);
    let (mut app, _) = gate_app(&_home, FetchStatus::Fresh);

    app.apply_usage();

    assert!(
        !app.bell_fired.is_ringing(GATE_PROFILE),
        "an hour-old figure is stale even under a Fresh status"
    );
}

#[test]
fn apply_usage_skips_the_bell_on_a_lapsed_five_hour_window() {
    let _home = crate::testutil::HomeSandbox::new();
    let (mut app, _) = gate_app(&_home, FetchStatus::Fresh);
    let lapsed = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
    app.usage_store
        .lock()
        .expect("usage_store mutex poisoned")
        .insert(
            GATE_PROFILE.to_string(),
            UsageInfo {
                five_hour: Some(UsageWindow {
                    utilization: GATE_UTIL,
                    resets_at: Some(lapsed),
                }),
                ..UsageInfo::default()
            },
        );

    app.apply_usage();

    assert!(
        !app.bell_fired.is_ringing(GATE_PROFILE),
        "a 5h window past its reset is no figure to ring on"
    );
}

#[test]
fn apply_usage_fresh_status_fires_bell_and_never_writes_history() {
    let _home = crate::testutil::HomeSandbox::new();
    let prior = seed_prior_history_entry();
    let (mut app, history_path) = gate_app(&_home, FetchStatus::Fresh);

    app.apply_usage();

    // Bell arm: util(80) >= threshold(70), not yet ringing → fires.
    assert!(
        app.bell_fired.is_ringing(GATE_PROFILE),
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

/// The wallet series' TUI wiring, end to end on disk: `App::new` loads it at
/// bootstrap, and `apply_usage` re-reads it when the file's content moves —
/// the carrier the usage tab's balance-row clause and the overview's drains
/// line read, so a regression here silences both surfaces with no other
/// signal.
#[test]
fn wallet_series_loads_at_bootstrap_and_reloads_on_change() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["ds-wire"]);
    let name = crate::profile::ProfileName::from("ds-wire");
    let now = crate::usage::now_ms();
    let drain = |amount: f64, hours_ago: u64| {
        crate::profile::append_wallet_readings_at(
            &name,
            &wire_wallet_stats(amount),
            now - hours_ago * 3_600_000,
        );
    };
    drain(100.0, 12);
    drain(90.0, 11);
    drain(80.0, 1);

    let profile = crate::profile::Profile::new(
        "ds-wire".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );
    let mut app = app_with(vec![profile]);
    assert_eq!(
        app.wallet_cache.get("ds-wire").map(Vec::len),
        Some(5),
        "bootstrap loads the series: first reading + two bridge pairs",
    );

    // A landing fetch appends; the content change is the reload signal.
    drain(70.0, 0);
    app.apply_usage();
    assert_eq!(
        app.wallet_cache.get("ds-wire").map(Vec::len),
        Some(7),
        "apply_usage re-reads a moved wallet history file",
    );
}

/// The reload signal is the file's CONTENT, not its mtime: on windows NTFS
/// quantizes file times, so a rapid append can land with a byte-identical
/// LastWriteTime (measured 2026-09-13: 4/10 real-box runs) and an mtime-only
/// gate serves a stale series. This pin restores the recorded mtime after the
/// append — the exact collision — and still requires the re-read.
#[test]
fn wallet_series_reloads_when_an_append_keeps_the_mtime() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["ds-wire"]);
    let name = crate::profile::ProfileName::from("ds-wire");
    let now = crate::usage::now_ms();
    let drain = |amount: f64, hours_ago: u64| {
        crate::profile::append_wallet_readings_at(
            &name,
            &wire_wallet_stats(amount),
            now - hours_ago * 3_600_000,
        );
    };
    drain(100.0, 12);
    drain(90.0, 11);
    drain(80.0, 1);

    let profile = crate::profile::Profile::new(
        "ds-wire".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );
    let mut app = app_with(vec![profile]);
    assert_eq!(
        app.wallet_cache.get("ds-wire").map(Vec::len),
        Some(5),
        "bootstrap loads the series: first reading + two bridge pairs",
    );
    let path = crate::profile::profile_wallet_history_path(&name).expect("wallet path");
    let recorded = std::fs::metadata(&path)
        .expect("wallet file stats")
        .modified()
        .expect("wallet mtime");

    // A landing fetch appends two lines; the gate must flip on the content,
    // even when the quantized mtime comes back exactly as recorded.
    drain(70.0, 0);
    crate::testutil::set_mtime(&path, recorded);
    app.apply_usage();
    assert_eq!(
        app.wallet_cache.get("ds-wire").map(Vec::len),
        Some(7),
        "apply_usage re-reads a wallet series whose append kept the mtime",
    );
}

/// The hash half: a rewrite that lands the SAME byte length with the recorded
/// mtime must still flip the gate — a length-only signal cannot see it.
#[test]
fn wallet_series_reloads_when_a_same_length_rewrite_keeps_the_mtime() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["ds-wire"]);
    let name = crate::profile::ProfileName::from("ds-wire");
    let now = crate::usage::now_ms();
    let drain = |amount: f64, hours_ago: u64| {
        crate::profile::append_wallet_readings_at(
            &name,
            &wire_wallet_stats(amount),
            now - hours_ago * 3_600_000,
        );
    };
    drain(100.0, 12);
    drain(90.0, 11);
    drain(80.0, 1);

    let profile = crate::profile::Profile::new(
        "ds-wire".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );
    let mut app = app_with(vec![profile]);
    assert_eq!(app.wallet_cache.get("ds-wire").map(Vec::len), Some(5));
    let path = crate::profile::profile_wallet_history_path(&name).expect("wallet path");
    let recorded = std::fs::metadata(&path)
        .expect("wallet file stats")
        .modified()
        .expect("wallet mtime");

    // The newest line's amount moves 80.0 -> 81.0: bytes swapped, byte length
    // kept, mtime restored to what the bootstrap recorded.
    let body = std::fs::read_to_string(&path).expect("wallet body");
    assert_eq!(
        body.matches("\"amount\":80.0").count(),
        1,
        "the fixture amount must be unique for a single-line swap"
    );
    let rewritten = body.replace("\"amount\":80.0", "\"amount\":81.0");
    assert_eq!(
        rewritten.len(),
        body.len(),
        "the swap must keep the byte length"
    );
    std::fs::write(&path, &rewritten).expect("rewrite wallet body");
    crate::testutil::set_mtime(&path, recorded);
    app.apply_usage();
    assert_eq!(
        app.wallet_cache
            .get("ds-wire")
            .and_then(|v| v.last())
            .map(|s| s.amount),
        Some(81.0),
        "apply_usage re-reads a same-length rewrite that kept the mtime",
    );
}

/// The length half: a retention trim removes old lines from the HEAD (the
/// real prune shape), so the tail-window hash can stay byte-identical while
/// the length shrinks — the gate must still flip.
#[test]
fn wallet_series_reloads_when_a_head_trim_keeps_the_tail_and_mtime() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["ds-wire"]);
    let name = crate::profile::ProfileName::from("ds-wire");
    let now = crate::usage::now_ms();
    for i in 0..40u64 {
        crate::profile::append_wallet_readings_at(
            &name,
            &wire_wallet_stats(100.0 + i as f64),
            now - (40 - i) * 3_600_000,
        );
    }

    let profile = crate::profile::Profile::new(
        "ds-wire".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );
    let mut app = app_with(vec![profile]);
    // 40 changed readings: the first writes one line, each later one a bridge
    // pair — 79 lines, far past the 256-byte tail window.
    assert_eq!(app.wallet_cache.get("ds-wire").map(Vec::len), Some(79));
    let path = crate::profile::profile_wallet_history_path(&name).expect("wallet path");
    let recorded = std::fs::metadata(&path)
        .expect("wallet file stats")
        .modified()
        .expect("wallet mtime");

    let body = std::fs::read_to_string(&path).expect("wallet body");
    assert!(
        body.len() > 256,
        "the series must exceed the tail window for this pin"
    );
    let first_end = body.find('\n').expect("at least one line");
    let trimmed = body[first_end + 1..].to_string();
    assert_eq!(
        &trimmed[trimmed.len() - 256..],
        &body[body.len() - 256..],
        "the trim must leave the tail window byte-identical, or the pin \
         stops pinning the length half"
    );
    std::fs::write(&path, &trimmed).expect("rewrite wallet body");
    crate::testutil::set_mtime(&path, recorded);
    app.apply_usage();
    assert_eq!(
        app.wallet_cache.get("ds-wire").map(Vec::len),
        Some(78),
        "apply_usage re-reads a head-trimmed wallet series whose tail and \
         mtime kept their values",
    );
}

fn wire_wallet_stats(amount: f64) -> crate::providers::ThirdPartyStats {
    crate::providers::ThirdPartyStats {
        is_available: true,
        rows: vec![crate::providers::StatRow {
            label: "api balance".to_string(),
            value: format!("{amount:.2} CNY"),
            kind: crate::providers::StatRowKind::Body,
        }],
        bars: vec![],
        plan: None,
        endpoint: None,
        best_effort: false,
    }
}

/// #74 degraded cue, FEED half: `apply_usage` derives `usage_stale` off the
/// DISK body's `fetched_at` vs `stale_after_ms`, with the spent-account
/// exemption reading the disk cache too (never the live store — a spent
/// account the scheduler dropped from its due set keeps its store entry, so
/// the two sources disagree exactly on the exempted state). The render pins
/// in `tui_render_usage.rs` hold only if this derivation is right.
#[test]
fn apply_usage_feeds_usage_stale_off_the_disk_cache_age() {
    let stale_for = |disk: UsageInfo| {
        let _home = crate::testutil::HomeSandbox::new();
        let mut app = {
            let mut profile =
                crate::testutil::blank_profile(&crate::profile::ProfileName::from(GATE_PROFILE));
            profile.bell_threshold = None;
            App::new(crate::profile::AppConfig {
                state: crate::profile::AppState {
                    profiles: vec![GATE_PROFILE.into()],
                    // The spent skip exists only under the opt-out (the
                    // default is ON), so the exempt arm below needs it OFF.
                    refresh_spent_accounts: false,
                    ..crate::profile::AppState::default()
                },
                profiles: vec![profile],
            })
        };
        // The live store carries a NON-maxed body while the disk cache is
        // maxed (spent): the exemption must read the disk side, so a
        // store-reading derivation flips stale on for a spent account and
        // reds the exempt arm below.
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        {
            let mut store = app.usage_store.lock().expect("usage_store mutex poisoned");
            store.insert(
                GATE_PROFILE.to_string(),
                UsageInfo {
                    five_hour: Some(UsageWindow {
                        utilization: 42.0,
                        resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
                    }),
                    ..UsageInfo::default()
                },
            );
        }
        crate::testutil::register_names(&[GATE_PROFILE]);
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from(GATE_PROFILE),
            crate::profile_cache::USAGE_CACHE_FILE,
            &disk,
        );
        app.apply_usage();
        {
            let cfg = app.config();
            cfg.profiles
                .iter()
                .find(|p| p.name.as_str() == GATE_PROFILE)
                .expect("profile present")
                .usage_stale
        }
    };
    let interval = crate::profile::AppState::default().refresh_interval_ms;
    let body = |util: f64, resets_at: &str, fetched_at: Option<u64>| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: util,
            resets_at: Some(resets_at.to_string()),
        }),
        fetched_at,
        ..UsageInfo::default()
    };
    let dated = |age_ms: u64, util: f64| {
        body(
            util,
            "2999-01-01T00:00:00+00:00",
            Some(crate::usage::now_ms() - age_ms),
        )
    };
    let threshold = crate::profile_json::stale_after_ms(interval);

    assert!(
        !stale_for(dated(threshold / 2, 42.0)),
        "a cache under the threshold must not read stale"
    );
    assert!(
        stale_for(dated(threshold + 60_000, 42.0)),
        "a cache past the threshold must read stale"
    );
    // `windows_maxed` keys on the DISK body (100%, a far-future reset): the
    // exempt arm holds even at an age far past the threshold, and holds
    // against the live store's non-maxed body.
    assert!(
        !stale_for(dated(threshold + 60_000, 100.0)),
        "a live-maxed window is exempt: its figure cannot change by polling"
    );
    // The age rides the BODY. An undated one is stale on its own, and only
    // this arm separates the contract from the cache-mtime derivation it
    // replaced: the fixture writes the file NOW, so an mtime reading calls it
    // fresh.
    assert!(
        stale_for(body(42.0, "2999-01-01T00:00:00+00:00", None)),
        "a body nothing can date reads stale"
    );
    // ...and the verdict qualifies a figure, so a body whose only window has
    // lapsed carries no marker however undatable it is.
    assert!(
        !stale_for(body(42.0, "2000-01-01T00:00:00+00:00", None)),
        "an all-lapsed body publishes no row for a marker to qualify"
    );
}

/// The read half: the log is written by whichever process holds the fetch lease,
/// so a file that appeared or changed since the last look must be picked up off
/// its content. Written here AFTER the `App` is built, standing in for the
/// daemon landing a sample while the TUI is open.
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
    std::fs::write(&history_path, grown).expect("append as the other process");
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

/// The history leg's quantized-mtime pin: an external writer appends while
/// the file's mtime stays at the value the last read recorded — the exact
/// windows NTFS collision the wallet pins reproduce for the wallet leg.
#[test]
fn apply_usage_reloads_history_when_an_append_keeps_the_mtime() {
    let _home = crate::testutil::HomeSandbox::new();
    let (mut app, history_path) = gate_app(&_home, FetchStatus::Fresh);
    let prior = seed_prior_history_entry();
    app.apply_usage();
    let first_read = app
        .history_cache
        .get(GATE_PROFILE)
        .expect("the seeded log must be read")
        .len();
    let recorded = std::fs::metadata(&history_path)
        .expect("history file stats")
        .modified()
        .expect("history mtime");

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
    std::fs::write(&history_path, grown).expect("append as the other process");
    crate::testutil::set_mtime(&history_path, recorded);
    app.apply_usage();

    let regrown = app
        .history_cache
        .get(GATE_PROFILE)
        .expect("the log must still be cached");
    assert_eq!(
        regrown.len(),
        first_read + 1,
        "an append that kept the mtime must still re-read (got {regrown:?})",
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
        !app.bell_fired.is_ringing(GATE_PROFILE),
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
        !app.bell_fired.is_ringing(GATE_PROFILE),
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
    assert!(
        card_edit_row(&app) == Some(super::FallbackRow::WeeklyAt),
        "⏎ opens the field"
    );
    for c in ['9', '0'] {
        super::handle_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_key(&mut app, key(KeyCode::Enter));
    assert!(app.fallback_edit.is_none(), "⏎ closes the field");
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
    assert!(
        card_edit_row(&app) == Some(super::FallbackRow::WeeklyAt),
        "invalid stays open"
    );
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
        app.fallback_edit.is_none(),
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
            ..crate::profile::OAuthToken::default_extra()
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
        harness: crate::harness::Harness::Claude,
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

    // Both are oauth with a login `/usage` polls, so the add is same-kind to a
    // homogeneous chain of measurable members and must skip every modal.
    let profiles = vec![polled_profile("alice"), polled_profile("carol")];
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

/// A blank profile with an OAuth login, the one subscription shape `/usage`
/// polls (so `no_usage_source` is false for it).
fn polled_profile(name: &str) -> crate::profile::Profile {
    let mut p = crate::testutil::blank_profile(&crate::profile::ProfileName::from(name));
    p.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    p
}

/// Drive the `+ add` Enter on the first candidate of a chain holding `chain`.
fn add_first_candidate(profiles: Vec<crate::profile::Profile>, chain: Vec<&str>) -> App {
    use crate::profile::{AppConfig, AppState};
    let names: Vec<crate::profile::ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: names,
            fallback_chain: chain.into_iter().map(Into::into).collect(),
            ..AppState::default()
        },
        profiles,
    });
    app.tab = super::Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    // `+ add` follows the members; its picker's first row is the first candidate.
    let add_row = app.config().state.fallback_chain.len();
    app.chain_cursor = add_row;
    app.fallback_detail_cursor = 0;
    super::handle_fallback_add_key(&mut app, crate::testutil::key(super::KeyCode::Enter));
    app
}

fn raised_confirm(app: &App) -> &super::ConfirmState {
    app.modals
        .last()
        .and_then(|m| match m {
            super::Modal::Confirm(s) => Some(s),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a confirm modal, got {:?}", app.modals))
}

// #110: adding a member clauth cannot measure asks first, with the production
// copy read back off the raised modal, and the member stays out until the
// confirm runs.
#[test]
fn fallback_add_enter_raises_confirm_modal_when_the_candidate_has_no_usage_source() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = add_first_candidate(
        vec![
            polled_profile("alice"),
            crate::testutil::blank_profile(&crate::profile::ProfileName::from("st")),
        ],
        vec!["alice"],
    );
    let confirm = raised_confirm(&app);
    assert!(
        matches!(&confirm.on_confirm, super::ConfirmAction::AddChainCandidate(n) if n == "st"),
        "confirm carries AddChainCandidate(\"st\"), got {:?}",
        confirm.on_confirm
    );
    assert_eq!(confirm.message, "clauth can't read this account's usage.");
    assert_eq!(
        confirm.detail.as_deref(),
        Some("once it's active, clauth won't switch away from it on its own."),
    );
    assert!(
        !app.config().state.fallback_chain.iter().any(|n| n == "st"),
        "the candidate must not enter the chain until the confirm runs"
    );
}

// An add that would mix kinds AND lands a member clauth cannot measure keeps
// the mix modal: it is the one about sessions getting stuck, and the
// unmeasured member's marker still shows once it is in.
#[test]
fn fallback_add_enter_keeps_the_mix_modal_when_both_warnings_apply() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut keyed = crate::testutil::blank_profile(&crate::profile::ProfileName::from("keyed"));
    keyed.api_key = Some("sk-test".to_string());
    let app = add_first_candidate(
        vec![
            keyed,
            crate::testutil::blank_profile(&crate::profile::ProfileName::from("st")),
        ],
        vec!["keyed"],
    );
    assert_eq!(
        raised_confirm(&app).message,
        "mixing api-key and oauth accounts can leave sessions stuck on the api account.",
    );
}

// ── `[ daemon ]` header chip at startup ─────────────────────────────────────

/// The chip's health must be PROBED at construct, never seeded with a constant.
/// The first paint happens before any `on_tick` and `poll_daemon_health` is
/// throttled to 1 Hz, so a seeded `Absent` renders "no daemon runs" as fact for
/// the first second of every launch while one is live. Two legs on one sandbox:
/// with the singleton flock held the seed reads non-`Absent` (the chip colors),
/// with it released `Absent` (the chip dims) — so no constant seed passes both.
#[test]
fn construct_probes_the_daemon_health_instead_of_seeding_a_constant() {
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
        "no holder → the seed dims the chip, proving the probe reads the lock"
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
                ("start daemon", Some('t')),
                ("start shunt", None),
            ],
            "{focus:?} carries the account-scoped trio, then the daemon verb",
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
        [
            ("apply preset", Some('p')),
            ("start daemon", Some('s')),
            ("start shunt", Some('t')),
        ],
        "`+ new` offers apply preset, then the daemon verb",
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
            ..crate::profile::OAuthToken::default_extra()
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
    target.models.fable = Some("old-fable".to_string());
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
        Some("replaces base url, model, fable, opus."),
        "the warning names the fields in the Setup rows' order",
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

/// A preset applied while the account's detail pane is open reshapes the row
/// list above the cursor (the endpoint and override rows); the cursor stays on
/// the row it was on.
#[test]
fn a_preset_applied_from_the_open_detail_pane_keeps_the_cursor_on_its_row() {
    use super::{
        ActionMenuAction, ConfigRow, Modal, Tab, config_rows, dispatch_action_menu_action,
        handle_key, run_confirm_action,
    };
    use crate::profile::Profile;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut target = Profile::new("target".to_string(), None, None);
    target.models.opus = Some("old-opus".to_string());
    crate::profile::save_profile(&target).expect("save target");

    let mut app = app_with(vec![target]);
    app.tab = Tab::Setup;
    app.profile_cursor = 0;
    super::enter_config_detail(&mut app);
    let before = config_rows(&app);
    app.config_action_cursor = before
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("login row");

    dispatch_action_menu_action(&mut app, ActionMenuAction::ApplyPreset);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter)); // cursor 0 = DeepSeek
    let Some(Modal::Confirm(state)) = app.modals.pop() else {
        panic!("the set opus override raises the overwrite warning");
    };
    run_confirm_action(&mut app, state.on_confirm);

    let after = config_rows(&app);
    assert_ne!(
        after.iter().position(|r| *r == ConfigRow::Login),
        before.iter().position(|r| *r == ConfigRow::Login),
        "precondition: the apply moved the login row",
    );
    assert_eq!(after.get(app.config_action_cursor), Some(&ConfigRow::Login));
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
        console_login_target(&app, &crate::profile::ProfileName::from("qwen-intl")),
        Some((ConsoleSite::International, "ap-southeast-1"))
    );
    assert_eq!(
        console_login_target(&app, &crate::profile::ProfileName::from("qwen-cn")),
        Some((ConsoleSite::Domestic, "cn-beijing"))
    );

    for other in ["oauth", "deepseek", "proxy", "missing"] {
        assert_eq!(
            console_login_target(&app, &crate::profile::ProfileName::from(other)),
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
        console_login_target(&app, &crate::profile::ProfileName::from("swapped"))
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

// ── herdr row (Services tab) ──────────────────────────────────────────────────

use crate::herdr::{ConfigStatus, HerdrProbe, RegistryEntry, SidebarState};

fn herdr_entry(enabled: bool, min: Option<&str>, warnings: Vec<&str>) -> RegistryEntry {
    RegistryEntry {
        enabled,
        version: Some("0.1.0".into()),
        min_herdr_version: min.map(str::to_string),
        plugin_root: None,
        source_kind: Some("github".into()),
        resolved_commit: None,
        source_owner: None,
        source_repo: None,
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
    assert!(check.detail.iter().any(|l| *l
        == format!(
            "  {}",
            crate::tui::render::prose::cmd("clauth herdr install")
        )));
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
        Some(super::ServiceFix::HealHerdrConfig(p)) if p == &std::path::PathBuf::from("/tmp/herdr/config.toml")
    ));
}

/// The whole herdr detail in cloudy's order: the linked checkout's root first,
/// then the plugin, the herdr version, key, sidebar, and the fix block last.
#[test]
fn herdr_check_reads_root_first_and_the_fix_last() {
    let mut entry = herdr_entry(true, Some("0.8.0"), vec![]);
    entry.source_kind = Some("local".into());
    entry.plugin_root = Some("/home/u/src/clauth/herdr-plugin".into());
    let check = super::herdr_check(
        &herdr_probe(Some("0.8.2"), Some(entry), None),
        Some(&herdr_config(true, None, SidebarState::Templated)),
    );
    assert_eq!(
        check.detail,
        vec![
            "root: /home/u/src/clauth/herdr-plugin".to_string(),
            "plugin: linked (local)".to_string(),
            "herdr: 0.8.2".to_string(),
            "key: not bound".to_string(),
            "sidebar: templated".to_string(),
            "adds the keybinding and sidebar row to herdr's config".to_string(),
            "f  heal herdr config".to_string(),
        ],
        "every line in order, by equality"
    );
    assert_eq!(
        check.problems.iter().map(|p| p.line).collect::<Vec<_>>(),
        vec![6],
        "the fix line's index follows the reorder"
    );

    let check = super::herdr_check(
        &herdr_probe(Some("0.8.2"), None, None),
        Some(&healthy_herdr_config()),
    );
    assert_eq!(
        check.detail,
        vec![
            "plugin: not installed".to_string(),
            "herdr: 0.8.2".to_string(),
            String::new(),
            format!(
                "  {}",
                crate::tui::render::prose::cmd("clauth herdr install")
            ),
        ],
        "an uninstalled plugin leads with its verdict"
    );

    // The registry warnings follow `plugin`, the version follows them, and the
    // floor warning sits under the version it compares.
    let mut entry = herdr_entry(true, Some("0.8.0"), vec!["plugin root is gone"]);
    entry.source_kind = Some("local".into());
    entry.plugin_root = Some("/home/u/src/clauth/herdr-plugin".into());
    let check = super::herdr_check(
        &herdr_probe(Some("0.7.0"), Some(entry.clone()), None),
        Some(&healthy_herdr_config()),
    );
    assert_eq!(
        check.detail,
        vec![
            "root: /home/u/src/clauth/herdr-plugin".to_string(),
            "plugin: linked (local)".to_string(),
            "  plugin root is gone".to_string(),
            "herdr: 0.7.0".to_string(),
            "plugin needs herdr 0.8.0 or newer".to_string(),
            "key: prefix+a".to_string(),
            "sidebar: templated".to_string(),
        ],
        "warnings, version and floor warning in order"
    );

    // A probe error sits under the version, with or without a registry entry.
    let check = super::herdr_check(
        &herdr_probe(Some("0.8.2"), Some(entry), Some("boom: it broke")),
        Some(&healthy_herdr_config()),
    );
    assert_eq!(
        check.detail[..5].to_vec(),
        vec![
            "root: /home/u/src/clauth/herdr-plugin".to_string(),
            "plugin: linked (local)".to_string(),
            "  plugin root is gone".to_string(),
            "herdr: 0.8.2".to_string(),
            "  boom: it broke".to_string(),
        ],
        "the error follows the version when an entry exists"
    );
    let check = super::herdr_check(
        &herdr_probe(Some("0.8.2"), None, Some("boom: it broke")),
        Some(&healthy_herdr_config()),
    );
    assert_eq!(
        check.detail,
        vec!["herdr: 0.8.2".to_string(), "  boom: it broke".to_string()],
        "with no entry the version and the error are the whole detail"
    );
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
    app.services.herdr = Some(None);
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        !app.services.checks.iter().any(|c| c.label == "herdr"),
        "a resolved-but-absent herdr must not render a row, got {:?}",
        app.services
            .checks
            .iter()
            .map(|c| c.label)
            .collect::<Vec<_>>()
    );
}

/// Drain the Services probe workers' results the way the tick does, waiting up
/// to 10 s for every running one to land.
fn await_service_probes(app: &mut App) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.services.herdr_probe.running
        || app.services.standalone_probe.running
        || app.services.move_plan_probe.running
        || app.services.pool_probe.running
        || app.services.shunt_action.running
    {
        assert!(
            std::time::Instant::now() < deadline,
            "a Services probe did not land within 10 s"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_service_probes(app);
    }
}

/// While the Services tab shows a standalone shunt's claim, the readout is
/// re-probed once the recheck interval passes and the claim drops once that
/// shunt stops answering, with no `r`; nothing re-probes inside the interval,
/// on another tab, under a modal, or once no claim shows.
#[test]
fn a_standalone_claim_is_rechecked_while_the_services_tab_shows_it() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    static ANSWERS: AtomicBool = AtomicBool::new(true);
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    fn stub() -> super::StandaloneShunt {
        RUNS.fetch_add(1, Ordering::SeqCst);
        super::StandaloneShunt {
            found: Some(std::path::PathBuf::from("/cfg/shunt.toml")),
            answer: ANSWERS
                .load(Ordering::SeqCst)
                .then(|| ("0.49.1".to_string(), crate::gateway::SHUNT_DEFAULT_BIND)),
            ..super::StandaloneShunt::default()
        }
    }
    let ago = |secs: u64| {
        Instant::now()
            .checked_sub(Duration::from_secs(secs))
            .unwrap()
    };
    let home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.standalone_probe.prober = Some(stub);
    app.services.standalone_probe.start();
    await_service_probes(&mut app);
    let claim = |app: &App| {
        app.services
            .standalone
            .as_ref()
            .and_then(|s| s.answer.clone())
    };
    let claim_line = "standalone shunt 0.49.1 answers on 127.0.0.1:3001".to_string();
    let card_claims = |app: &App| {
        app.services
            .checks
            .iter()
            .find(|c| c.label == "shunt")
            .is_some_and(|c| c.detail.contains(&claim_line))
    };
    assert!(claim(&app).is_some(), "the stub's shunt answers");
    assert!(card_claims(&app), "the card shows the claim");
    let runs = RUNS.load(Ordering::SeqCst);

    // Each poll below runs the 1 s Services refresh, its own throttle passed;
    // `refresh` leaves the recheck timer where the last recheck put it.
    let refresh = |app: &mut App| {
        app.last_services_refresh = ago(2);
        super::poll_services_refresh(app);
    };
    let poll = |app: &mut App, since_recheck: u64| {
        app.services.last_standalone_recheck = ago(since_recheck);
        refresh(app);
    };

    poll(&mut app, 3);
    assert!(
        !app.services.standalone_probe.running,
        "inside the interval"
    );
    app.tab = super::Tab::Overview;
    poll(&mut app, 6);
    assert!(!app.services.standalone_probe.running, "on another tab");
    app.tab = super::Tab::Services;
    app.modals.push(super::Modal::Help);
    poll(&mut app, 6);
    assert!(!app.services.standalone_probe.running, "under a modal");
    app.modals.clear();
    // An adopted gateway hides the readout, cached claim and all.
    crate::testutil::write_adopted_record(&home, true, "[server]\n");
    poll(&mut app, 6);
    assert!(
        !app.services.standalone_probe.running,
        "with a gateway adopted"
    );
    std::fs::remove_file(home.home().join(".clauth").join("gateway.toml")).unwrap();
    // The claim renders only in the `shunt` card: the cursor on another row
    // takes it off screen.
    let shunt_row = app.services.cursor;
    app.services.cursor = app
        .services
        .checks
        .iter()
        .position(|c| c.label == "delegates")
        .expect("a delegates row");
    poll(&mut app, 6);
    assert!(
        !app.services.standalone_probe.running,
        "with another row selected"
    );
    app.services.cursor = shunt_row;
    assert_eq!(RUNS.load(Ordering::SeqCst), runs, "no probe ran");

    // A recheck that still hears the shunt keeps the claim and restarts the
    // interval: the next second's refresh probes nothing.
    poll(&mut app, 6);
    assert!(
        app.services.standalone_probe.running,
        "a recheck past the interval"
    );
    await_service_probes(&mut app);
    assert!(card_claims(&app), "the shunt still answers");
    refresh(&mut app);
    assert!(
        !app.services.standalone_probe.running,
        "the interval restarted"
    );
    assert_eq!(RUNS.load(Ordering::SeqCst), runs + 1);

    ANSWERS.store(false, Ordering::SeqCst);
    poll(&mut app, 6);
    assert!(
        app.services.standalone_probe.running,
        "past the interval, on the tab"
    );
    await_service_probes(&mut app);
    assert_eq!(claim(&app), None, "the claim dropped with no `r`");
    assert!(!card_claims(&app), "and the card no longer shows it");
    assert_eq!(RUNS.load(Ordering::SeqCst), runs + 2);

    poll(&mut app, 6);
    assert!(
        !app.services.standalone_probe.running,
        "no claim, no re-probe"
    );
}

/// `r` during a probe already in flight queues one follow-up run, started
/// when the held run lands, so the toast's "re-running service checks" holds
/// for a change made just before the key. A second `r` queues no second run.
/// Both Services workers share it.
#[test]
fn r_during_a_running_probe_queues_one_follow_up_run() {
    use ratatui::crossterm::event::KeyCode;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static HERDR_RUNS: AtomicUsize = AtomicUsize::new(0);
    static STANDALONE_RUNS: AtomicUsize = AtomicUsize::new(0);
    fn herdr_stub() -> Option<crate::herdr::HerdrProbe> {
        HERDR_RUNS.fetch_add(1, Ordering::SeqCst);
        None
    }
    fn standalone_stub() -> super::StandaloneShunt {
        STANDALONE_RUNS.fetch_add(1, Ordering::SeqCst);
        super::StandaloneShunt::default()
    }
    let _home = crate::testutil::HomeSandbox::new();
    HERDR_RUNS.store(0, Ordering::SeqCst);
    STANDALONE_RUNS.store(0, Ordering::SeqCst);

    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.herdr_probe.prober = Some(herdr_stub);
    app.services.standalone_probe.prober = Some(standalone_stub);
    // Both runs held in flight: set by hand, so neither stub runs for them.
    app.services.herdr_probe.running = true;
    app.services.standalone_probe.running = true;
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Char('r')));
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Char('r')));
    assert_eq!(
        (
            HERDR_RUNS.load(Ordering::SeqCst),
            STANDALONE_RUNS.load(Ordering::SeqCst)
        ),
        (0, 0),
        "no run starts beside the one in flight"
    );

    app.services
        .herdr_probe
        .tx
        .send(Some(healthy_herdr_probe()))
        .expect("send");
    app.services
        .standalone_probe
        .tx
        .send(super::StandaloneShunt::default())
        .expect("send");
    super::drain_service_probes(&mut app);
    assert!(
        app.services.herdr_probe.running && app.services.standalone_probe.running,
        "each held run's landing starts its follow-up"
    );
    assert!(
        app.services.herdr.is_none() && app.services.standalone.is_none(),
        "a held run's result predates the `r` and is dropped"
    );
    await_service_probes(&mut app);
    assert_eq!(
        (
            HERDR_RUNS.load(Ordering::SeqCst),
            STANDALONE_RUNS.load(Ordering::SeqCst)
        ),
        (1, 1),
        "one follow-up each, however many `r`s"
    );
    assert!(
        matches!(app.services.herdr, Some(None)),
        "the follow-up's result is the one adopted"
    );

    // The per-tick refresh over a run in flight queues nothing: only `r` does.
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.herdr_probe.prober = Some(herdr_stub);
    app.services.standalone_probe.prober = Some(standalone_stub);
    app.services.herdr_probe.running = true;
    app.services.standalone_probe.running = true;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        !app.services.herdr_probe.rerun && !app.services.standalone_probe.rerun,
        "the refresh queues no follow-up"
    );
}

/// The standalone readout reads its bind from the env the adopt reads, which
/// a daemon start or stop flips: a cached readout is probed again on the flip.
#[test]
fn a_daemon_flip_probes_the_standalone_shunt_again() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    fn stub() -> super::StandaloneShunt {
        RUNS.fetch_add(1, Ordering::SeqCst);
        super::StandaloneShunt::default()
    }
    let _home = crate::testutil::HomeSandbox::new();
    RUNS.store(0, Ordering::SeqCst);
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.standalone_probe.prober = Some(stub);
    app.services.standalone = Some(super::StandaloneShunt::default());
    app.daemon_health = crate::daemon::DaemonHealth::Stale;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        !app.services.standalone_probe.running,
        "a cached readout with no flip is not probed again"
    );
    app.daemon_health = crate::daemon::DaemonHealth::Absent;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        app.services.standalone_probe.running,
        "the daemon going away probes again"
    );
    await_service_probes(&mut app);
    assert_eq!(RUNS.load(Ordering::SeqCst), 1);
}

/// A fresh/stale flap of one daemon keeps the env the readout resolves
/// against, so the cached readout is not probed again.
#[test]
fn a_fresh_stale_flap_does_not_probe_the_standalone_shunt_again() {
    fn stub() -> super::StandaloneShunt {
        super::StandaloneShunt::default()
    }
    let home = crate::testutil::HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(clauth.join("clauthd.pid"), "4100\n").unwrap();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.standalone_probe.prober = Some(stub);
    app.services.standalone = Some(super::StandaloneShunt::default());
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        !app.services.standalone_probe.running,
        "fixture control: the first recompute only records the source"
    );
    app.daemon_health = crate::daemon::DaemonHealth::Stale;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        !app.services.standalone_probe.running,
        "the same daemon going stale probes nothing again"
    );
}

/// A daemon restarted under another pid between two recomputes, with no
/// absent reading seen between them (the tab hidden, a modal open), is a new
/// env source: the cached readout is probed again with no `r`.
#[test]
fn a_daemon_restarted_unseen_probes_the_standalone_shunt_again() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    fn stub() -> super::StandaloneShunt {
        RUNS.fetch_add(1, Ordering::SeqCst);
        super::StandaloneShunt::default()
    }
    let home = crate::testutil::HomeSandbox::new();
    RUNS.store(0, Ordering::SeqCst);
    let pid_file = home.home().join(".clauth").join("clauthd.pid");
    std::fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    std::fs::write(&pid_file, "4100\n").unwrap();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.standalone_probe.prober = Some(stub);
    app.services.standalone = Some(super::StandaloneShunt::default());
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        !app.services.standalone_probe.running,
        "fixture control: the first recompute only records the source"
    );
    std::fs::write(&pid_file, "4200\n").unwrap();
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        app.services.standalone_probe.running,
        "a daemon under another pid probes again"
    );
    await_service_probes(&mut app);
    assert_eq!(RUNS.load(Ordering::SeqCst), 1);
}

/// A starting daemon stamps its pid before it records its env, so a readout
/// probed between the two resolved against no record: the record landing is
/// a new env source and probes again. A daemon restarted under its old pid
/// records another start time, a new source too.
#[test]
fn a_daemon_env_record_landing_or_changing_probes_the_standalone_shunt_again() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    fn stub() -> super::StandaloneShunt {
        RUNS.fetch_add(1, Ordering::SeqCst);
        super::StandaloneShunt::default()
    }
    let home = crate::testutil::HomeSandbox::new();
    RUNS.store(0, Ordering::SeqCst);
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(clauth.join("clauthd.pid"), "4100\n").unwrap();
    let write_record = |start: &str| {
        let record = serde_json::json!({
            "identity": crate::gateway::DaemonIdentity {
                pid: 4100,
                start: Some(start.to_string()),
            },
            "env": [],
        });
        std::fs::write(clauth.join("gateway-env.json"), record.to_string()).unwrap();
    };
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.standalone_probe.prober = Some(stub);
    app.services.standalone = Some(super::StandaloneShunt::default());
    app.daemon_health = crate::daemon::DaemonHealth::Stale;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        !app.services.standalone_probe.running,
        "fixture control: the first recompute only records the source"
    );
    write_record("a");
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        app.services.standalone_probe.running,
        "the record landing probes again"
    );
    await_service_probes(&mut app);
    write_record("b");
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        app.services.standalone_probe.running,
        "a record naming another start of the same pid probes again"
    );
    await_service_probes(&mut app);
    assert_eq!(RUNS.load(Ordering::SeqCst), 2);
}

/// An action landing while a plan run is in flight queues one follow-up. The
/// held run's plan predates the action, so it is dropped, never shown, and the
/// card shows nothing until the follow-up's plan lands.
#[test]
fn a_plan_landing_behind_a_queued_follow_up_is_dropped() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    fn stub() -> super::MovePlanOutcome {
        RUNS.fetch_add(1, Ordering::SeqCst);
        super::MovePlanOutcome::Refused("fresh".to_string())
    }
    let home = crate::testutil::HomeSandbox::new();
    RUNS.store(0, Ordering::SeqCst);
    crate::testutil::write_adopted_record(&home, true, "[server]\n");
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.move_plan_probe.prober = Some(stub);
    // A plan run and an action held in flight, set by hand so no stub runs
    // for them.
    app.services.move_plan_probe.running = true;
    app.services.shunt_action.running = true;
    app.services
        .shunt_action
        .tx
        .send(super::ShuntActionOutcome::NoOp)
        .expect("send");
    super::drain_service_probes(&mut app);
    assert_eq!(
        app.services.move_plan, None,
        "fixture control: the landed action drops the cached plan"
    );
    app.services
        .move_plan_probe
        .tx
        .send(super::MovePlanOutcome::Refused("stale".to_string()))
        .expect("send");
    super::drain_service_probes(&mut app);
    assert_eq!(
        app.services.move_plan, None,
        "the held run's plan predates the action"
    );
    assert!(
        app.services.move_plan_probe.running,
        "its landing starts the follow-up"
    );
    await_service_probes(&mut app);
    assert_eq!(
        app.services.move_plan,
        Some(super::MovePlanOutcome::Refused("fresh".to_string()))
    );
    assert_eq!(RUNS.load(Ordering::SeqCst), 1);
}

/// A drain adopts the newest of the results that landed since the last one,
/// and a drain with nothing landed leaves the rows as they are: no recompute.
#[test]
fn a_probe_drain_adopts_the_newest_result_and_skips_an_empty_one() {
    let _home = crate::testutil::HomeSandbox::new();
    let readout = |version: &str| super::StandaloneShunt {
        found: None,
        unread_bind: None,
        unread_config: None,
        unread_env: false,
        answer: Some((version.to_string(), "127.0.0.1:3067".parse().unwrap())),
    };
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.standalone_probe.running = true;
    for version in ["0.49.0", "0.49.1"] {
        app.services
            .standalone_probe
            .tx
            .send(readout(version))
            .expect("send");
    }
    super::drain_service_probes(&mut app);
    assert_eq!(
        app.services.standalone,
        Some(readout("0.49.1")),
        "the newest landing wins"
    );
    assert!(!app.services.standalone_probe.running);

    app.services.checks.clear();
    super::drain_service_probes(&mut app);
    assert!(
        app.services.checks.is_empty(),
        "a drain with nothing landed recomputes nothing"
    );
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        !app.services.checks.is_empty(),
        "fixture control: a recompute refills the rows"
    );
}

/// With no gateway adopted, entering the tab probes what runs standalone on a
/// worker, and the readout lands on the `shunt` row with no key; `r` probes
/// again. The adopted-gateway path never starts it.
#[test]
fn an_unadopted_gateway_row_probes_what_runs_standalone_with_no_key() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    fn stub() -> super::StandaloneShunt {
        RUNS.fetch_add(1, Ordering::SeqCst);
        super::StandaloneShunt {
            found: Some(std::path::PathBuf::from("/cfg/shunt.toml")),
            unread_bind: None,
            unread_config: None,
            unread_env: false,
            answer: Some(("0.49.1".to_string(), "127.0.0.1:3067".parse().unwrap())),
        }
    }
    let _home = crate::testutil::HomeSandbox::new();
    RUNS.store(0, Ordering::SeqCst);

    let mut app = bare_app();
    app.services.standalone_probe.prober = Some(stub);
    super::switch_tab(&mut app, super::Tab::Services);
    assert!(
        app.services.standalone_probe.running,
        "entering the tab starts the probe, no `r`"
    );
    await_service_probes(&mut app);
    let shunt = |app: &App| {
        app.services
            .checks
            .iter()
            .find(|c| c.label == "shunt")
            .expect("shunt row")
            .detail
            .clone()
    };
    assert_eq!(
        shunt(&app),
        vec![
            "state: not adopted".to_string(),
            "found: /cfg/shunt.toml".to_string(),
            "standalone shunt 0.49.1 answers on 127.0.0.1:3067".to_string(),
            "f  adopt config".to_string(),
        ],
    );
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        !app.services.standalone_probe.running,
        "the per-tick refresh reuses the cached readout"
    );
    super::recompute_services_checks(&mut app, true, super::ShuntRefresh::Keep);
    assert!(app.services.standalone_probe.running, "`r` probes again");
    await_service_probes(&mut app);
    assert_eq!(RUNS.load(Ordering::SeqCst), 2, "one run per trigger");

    // An adopted gateway: no standalone probe, and a readout cached from
    // before the adoption never reaches the row.
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let config = dir.join("shunt.toml");
    std::fs::write(&config, "port = 3001\n").expect("config");
    std::fs::write(
        dir.join("gateway.toml"),
        format!("config = {:?}\n", config.to_string_lossy()),
    )
    .expect("record");
    let mut app = bare_app();
    app.services.standalone_probe.prober = Some(stub);
    app.services.standalone = Some(super::StandaloneShunt {
        found: Some(std::path::PathBuf::from("/cfg/shunt.toml")),
        unread_bind: Some(super::UnreadBind::Yaml),
        unread_config: Some(super::UnreadConfig::Yaml),
        unread_env: false,
        answer: Some(("0.49.1".to_string(), "127.0.0.1:3067".parse().unwrap())),
    });
    super::recompute_services_checks(&mut app, true, super::ShuntRefresh::Keep);
    assert!(
        !app.services.standalone_probe.running,
        "an adopted gateway never probes for a standalone one"
    );
    assert!(
        shunt(&app).iter().any(|l| l.starts_with("config: ")),
        "fixture control: the row reads the adopted record: {:?}",
        shunt(&app)
    );
    assert!(
        !shunt(&app).iter().any(|l| l.starts_with("found")
            || l.starts_with("bind")
            || l.starts_with("standalone")),
        "{:?}",
        shunt(&app)
    );
}

/// Entering the Services tab with an installed herdr shows its row with no
/// key: the tab entry starts the probe (the real `herdr::probe`, over a stub
/// herdr pinned through `HERDR_BIN_PATH`) and the drain adopts it.
#[cfg(unix)]
#[test]
fn entering_the_services_tab_shows_the_herdr_row_with_no_key() {
    let home = crate::testutil::HomeSandbox::new();
    let shim = crate::testutil::stateful_heal_shim(home.home());
    let listing = crate::herdr::plugin_list_json(crate::herdr::GITHUB);
    let _env = crate::testutil::heal_env(&home, &shim, &listing, &listing, "", &[]);

    let mut app = bare_app();
    app.services.herdr_probe.prober = Some(crate::herdr::probe);
    super::switch_tab(&mut app, super::Tab::Services);
    assert!(
        app.services.herdr_probe.running,
        "entering the tab starts the probe, no `r`"
    );
    await_service_probes(&mut app);

    let herdr = app
        .services
        .checks
        .iter()
        .find(|c| c.label == "herdr")
        .unwrap_or_else(|| {
            panic!(
                "the herdr row shows once its probe lands, got {:?}",
                app.services
                    .checks
                    .iter()
                    .map(|c| c.label)
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        herdr
            .detail
            .iter()
            .any(|l| l == "plugin: installed (github)"),
        "the row reads the stub's listing: {:?}",
        herdr.detail
    );
}

/// Released by `a_slow_herdr_probe_leaves_the_first_paint_unblocked` once its
/// first paint is done; the gated prober waits on it (10 s at most).
static HERDR_GATE: (std::sync::Mutex<bool>, std::sync::Condvar) =
    (std::sync::Mutex::new(false), std::sync::Condvar::new());

/// How many times `gated_herdr_probe` started: the one-probe-at-a-time guard's
/// witness.
static GATED_PROBES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn gated_herdr_probe() -> Option<crate::herdr::HerdrProbe> {
    GATED_PROBES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (lock, cvar) = &HERDR_GATE;
    let open = lock.lock().expect("gate lock");
    let _open = cvar
        .wait_timeout_while(open, std::time::Duration::from_secs(10), |open| !*open)
        .expect("gate wait");
    Some(crate::herdr::HerdrProbe {
        version: Some("0.8.2".to_string()),
        entry: None,
        config_path: None,
        error: None,
    })
}

/// A probe that hangs never holds the first paint: entering the tab and
/// drawing a frame return while the probe is still held, with no herdr row
/// yet, and the row appears once the probe lands. While it is held, neither a
/// refresh tick's recompute nor `r` starts a second one beside it; `r` queues
/// one follow-up, which runs once the held probe lands.
#[test]
fn a_slow_herdr_probe_leaves_the_first_paint_unblocked() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.services.herdr_probe.prober = Some(gated_herdr_probe);

    let started = std::time::Instant::now();
    super::switch_tab(&mut app, super::Tab::Services);
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("terminal");
    term.draw(|f| crate::tui::render::draw(f, &app))
        .expect("first paint");
    let elapsed = started.elapsed();
    super::drain_service_probes(&mut app);

    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the tab entry and first paint waited on the held probe: {elapsed:?}"
    );
    assert!(
        app.services.herdr_probe.running,
        "the probe is still held after the first paint"
    );
    assert!(
        !app.services.checks.iter().any(|c| c.label == "herdr"),
        "no herdr row before the probe lands"
    );

    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    super::handle_key(
        &mut app,
        crate::testutil::key(ratatui::crossterm::event::KeyCode::Char('r')),
    );
    assert!(app.services.herdr_probe.running, "still the one held probe");
    assert!(
        app.services.herdr_probe.rerun,
        "`r` queued its probe behind the held one instead of beside it"
    );

    {
        let (lock, cvar) = &HERDR_GATE;
        *lock.lock().expect("gate lock") = true;
        cvar.notify_all();
    }
    await_service_probes(&mut app);
    assert!(
        app.services.checks.iter().any(|c| c.label == "herdr"),
        "the row appears once the probe lands"
    );
    super::join_test_workers();
    assert_eq!(
        GATED_PROBES.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the `r` held behind the first probe ran once after it landed"
    );
}

/// How many times `counting_herdr_probe` ran.
static COUNTED_PROBES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn counting_herdr_probe() -> Option<crate::herdr::HerdrProbe> {
    COUNTED_PROBES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Some(crate::herdr::HerdrProbe {
        version: Some("0.8.2".to_string()),
        entry: None,
        config_path: None,
        error: None,
    })
}

/// With a probe cached, a plain recompute starts none; `r` on the Services tab
/// probes herdr again.
#[test]
fn r_re_probes_herdr_on_the_services_tab() {
    use std::sync::atomic::Ordering;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.services.herdr_probe.prober = Some(counting_herdr_probe);
    let before = COUNTED_PROBES.load(Ordering::SeqCst);

    super::switch_tab(&mut app, super::Tab::Services);
    await_service_probes(&mut app);
    assert_eq!(
        COUNTED_PROBES.load(Ordering::SeqCst),
        before + 1,
        "tab entry probed once"
    );

    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        !app.services.herdr_probe.running,
        "a recompute with a probe cached starts none"
    );

    super::handle_key(
        &mut app,
        crate::testutil::key(ratatui::crossterm::event::KeyCode::Char('r')),
    );
    assert!(app.services.herdr_probe.running, "`r` starts a re-probe");
    await_service_probes(&mut app);
    assert_eq!(
        COUNTED_PROBES.load(Ordering::SeqCst),
        before + 2,
        "`r` probed again"
    );
}

fn panicking_herdr_probe() -> Option<crate::herdr::HerdrProbe> {
    panic!("the herdr probe blew up")
}

/// A probe that panics lands as "herdr does not resolve", so `herdr_probe.running`
/// never sticks and `r` stays live.
#[test]
fn a_panicking_herdr_probe_lands_as_no_herdr() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.services.herdr_probe.prober = Some(panicking_herdr_probe);
    super::switch_tab(&mut app, super::Tab::Services);
    await_service_probes(&mut app);
    assert!(
        matches!(app.services.herdr, Some(None)),
        "a panicking probe reads as no herdr"
    );
    assert!(
        !app.services.herdr_probe.running,
        "the in-flight flag cleared"
    );
}

/// The first herdr landing happens on the tick that drains its probe, or
/// never (cloudy, 2026-09-29). A modal opening before that drops the intent:
/// the probe landing under it leaves the cursor, and so does every refresh
/// after it closes. That holds for a modal already closed when the probe lands
/// (`a`, then esc: the action-menu path), which the old wait-for-a-modal-free
/// recompute landed. `f` on the list, like ↑↓/↵, is the user acting, so it
/// drops the intent too.
#[test]
fn a_modal_drops_the_herdr_landing_and_f_drops_it() {
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let probe = || crate::herdr::HerdrProbe {
        version: Some("0.8.2".to_string()),
        entry: None,
        config_path: None,
        error: None,
    };
    let land = |app: &mut App| {
        app.services.herdr_probe.running = true;
        app.services
            .herdr_probe
            .tx
            .send(Some(probe()))
            .expect("send");
        super::drain_service_probes(app);
        assert!(
            app.services.checks.iter().any(|c| c.label == "herdr"),
            "fixture control: the probe landed"
        );
    };

    // A modal still open when the probe lands.
    let mut app = bare_app().with_herdr_mode(true);
    assert!(app.services.land_on_herdr, "the landing is pending");
    let parked = app.services.cursor;
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Char('?')));
    assert!(
        !app.modals.is_empty(),
        "fixture control: the help modal is open"
    );
    assert!(
        !app.services.land_on_herdr,
        "opening a modal drops the intent"
    );
    land(&mut app);
    assert_eq!(
        app.services.cursor, parked,
        "the cursor stays under the modal"
    );
    assert_eq!(app.services.focus, super::ServicesFocus::List);
    app.modals.clear();
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert_eq!(
        app.services.cursor, parked,
        "the refresh after the modal closes does not land"
    );
    assert_eq!(app.services.focus, super::ServicesFocus::List);

    // A modal opened and closed before the probe lands.
    let mut app = bare_app().with_herdr_mode(true);
    let parked = app.services.cursor;
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Char('a')));
    assert!(
        matches!(app.modals.last(), Some(super::Modal::ActionMenu(_))),
        "fixture control: the action menu is open"
    );
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(app.modals.is_empty(), "fixture control: the menu closed");
    land(&mut app);
    assert_eq!(
        app.services.cursor, parked,
        "a landing after the user's own modal leaves the cursor"
    );
    assert_eq!(app.services.focus, super::ServicesFocus::List);

    let mut app = bare_app().with_herdr_mode(true);
    assert!(app.services.land_on_herdr, "the landing is pending");
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert!(
        !app.services.land_on_herdr,
        "`f` on the list is the user acting: the intent drops"
    );
}

/// With no modal, the landing is the first tick's after the probe result is
/// sent: `on_tick` drains the probe before its 1 s Services refresh, and the
/// refresh is held off here so only the drain can land it. A probe that
/// resolves with no herdr drops the intent, so a later resolve never lands.
#[test]
fn the_herdr_landing_is_the_draining_tick_or_never() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app().with_herdr_mode(true);
    assert!(app.services.land_on_herdr, "the landing is pending");
    app.services.herdr_probe.running = true;
    app.services
        .herdr_probe
        .tx
        .send(Some(healthy_herdr_probe()))
        .expect("send");
    app.last_services_refresh = std::time::Instant::now();
    super::on_tick(&mut app);
    assert_eq!(
        app.services.selected_check().map(|c| c.label),
        Some("herdr"),
        "the draining tick lands on the herdr row"
    );
    assert_eq!(app.services.focus, super::ServicesFocus::Detail);
    assert!(!app.services.land_on_herdr);

    let mut app = bare_app().with_herdr_mode(true);
    let parked = app.services.cursor;
    app.services.herdr_probe.running = true;
    app.services.herdr_probe.tx.send(None).expect("send");
    super::drain_service_probes(&mut app);
    assert!(
        !app.services.land_on_herdr,
        "a probe resolving with no herdr drops the intent"
    );
    app.services.herdr = Some(Some(healthy_herdr_probe()));
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        app.services.checks.iter().any(|c| c.label == "herdr"),
        "fixture control: the herdr row renders"
    );
    assert_eq!(app.services.cursor, parked, "a later resolve never lands");
    assert_eq!(app.services.focus, super::ServicesFocus::List);
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
        check.detail.iter().any(|l| *l
            == format!(
                "  could not run {}: boom",
                crate::tui::render::prose::cmd("herdr plugin list --json")
            )),
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
    app.tab = super::Tab::Services;
    app.services.herdr = Some(Some(healthy_herdr_probe()));
    app.services.herdr_config = Some(healthy_herdr_config());
    app.services.checks = vec![super::herdr_check(
        &healthy_herdr_probe(),
        Some(&healthy_herdr_config()),
    )];
    app.services.cursor = 0;
    app.services.focus = super::ServicesFocus::Detail;
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
    app.services.herdr_options_cursor = 1;
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
    app.services.herdr_options_cursor = 2;

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
        app.services.herdr_tag_draft.is_some(),
        "⏎ opens the typed editor"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Backspace));
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('3')));
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('0')));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(herdr_knobs().tag_watch_secs, 30, "the typed value commits");
    assert!(app.services.herdr_tag_draft.is_none());

    // An under-floor value keeps the editor open and persists nothing.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    handle_key(&mut app, crate::testutil::key(KeyCode::Backspace));
    handle_key(&mut app, crate::testutil::key(KeyCode::Backspace));
    handle_key(&mut app, crate::testutil::key(KeyCode::Backspace));
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('0')));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        app.services.herdr_tag_draft.is_some(),
        "an invalid value stays in the editor"
    );
    assert_eq!(herdr_knobs().tag_watch_secs, 30, "and persists nothing");
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(
        app.services.herdr_tag_draft.is_none(),
        "⎋ discards the draft"
    );
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
    app.services.herdr_options_cursor = 3;

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
        "printf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/herdr.log\"\nprintf '%s\\n' '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[{\"pane_id\":\"pane-a\",\"workspace_id\":\"wA\",\"tab_id\":\"tA\",\"agent_status\":\"idle\",\"focused\":false},{\"pane_id\":\"pane-b\",\"workspace_id\":\"wB\",\"tab_id\":\"tB\",\"agent_status\":\"idle\",\"focused\":false}]}}'",
    );
    let _report_shim = write_shim(
        tmp.path(),
        "report-profile.sh",
        "set -u\nprintf 'pane=%s event=%s context=%s\\n' \"$HERDR_PANE_ID\" \"$HERDR_PLUGIN_EVENT_JSON\" \"$HERDR_PLUGIN_CONTEXT_JSON\" >> \"$(dirname \"$0\")/report.log\"",
    );
    let _env = HerdrRuntimePin::new(&home, &herdr_shim, tmp.path(), true);
    let mut app = herdr_options_app();
    app.services.herdr_options_cursor = 3;

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
    app.services.herdr_options_cursor = 3;

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
    app.services.herdr_options_cursor = 4;

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
    app.services.herdr_config = Some(herdr_config(false, None, SidebarState::Absent));
    app.services.herdr_options_cursor = 5;

    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        app.modals.is_empty(),
        "no confirm while the config does not parse"
    );
    assert!(!herdr_knobs().delegate_row_text);
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        app.services.herdr_options_cursor, 0,
        "selection wraps past the inert row"
    );

    app.services.herdr_options_cursor = 4;
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
    app.tab = super::Tab::Services;
    let probe = crate::herdr::HerdrProbe {
        version: Some("0.8.0".to_string()),
        entry: Some(herdr_entry(true, Some("0.8.0"), vec![])),
        config_path: Some(path.to_path_buf()),
        error: None,
    };
    app.services.herdr = Some(Some(probe));
    app.services.focus = super::ServicesFocus::Detail;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    app.services.cursor = app
        .services
        .checks
        .iter()
        .position(|c| c.label == "herdr")
        .expect("herdr row present");
    app.services.herdr_options_cursor = 5;
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

/// RAII pin for `HERDR_BIN_PATH`, restored on drop (even on panic). The pin
/// itself is `crate::testutil::EnvPin`, which borrows the
/// [`crate::testutil::HomeSandbox`]: the env is a process-global serialized by
/// `HOME_TEST_LOCK`, which the sandbox holds.
#[cfg(unix)]
struct HerdrBinPin<'a> {
    _pin: crate::testutil::EnvPin<'a>,
}

#[cfg(unix)]
impl<'a> HerdrBinPin<'a> {
    fn new(home: &'a crate::testutil::HomeSandbox, bin: &std::path::Path) -> Self {
        Self {
            _pin: crate::testutil::EnvPin::new(home, &[("HERDR_BIN_PATH", Some(bin.as_os_str()))]),
        }
    }
}

/// RAII pin for the vars the knob push reads (`HERDR_ENV`, `HERDR_BIN_PATH`,
/// `HERDR_PLUGIN_ROOT`) plus stale event/context values the per-pane re-run
/// must NOT inherit — the push clears them, so a child carrying "stale-event"
/// reds the pin. With `herdr_env: false` only `HERDR_ENV` is removed: the
/// other vars stay pinned to prove the gate, not a missing path, is what
/// suppresses the spawn. Restored on drop (even on panic). Same contract as
/// `HerdrBinPin`: process-global env, serialized by `HOME_TEST_LOCK`, which
/// the borrowed sandbox holds.
struct HerdrRuntimePin<'a> {
    _pin: crate::testutil::EnvPin<'a>,
}

impl<'a> HerdrRuntimePin<'a> {
    fn new(
        home: &'a crate::testutil::HomeSandbox,
        bin: &std::path::Path,
        plugin_root: &std::path::Path,
        herdr_env: bool,
    ) -> Self {
        let bin = bin.as_os_str().to_string_lossy().into_owned();
        let plugin_root = plugin_root.as_os_str().to_string_lossy().into_owned();
        let herdr_env = herdr_env.then_some("1".to_string());
        Self {
            _pin: crate::testutil::EnvPin::new(
                home,
                &[
                    ("HERDR_ENV", herdr_env.as_deref().map(std::ffi::OsStr::new)),
                    ("HERDR_BIN_PATH", Some(std::ffi::OsStr::new(&bin))),
                    (
                        "HERDR_PLUGIN_ROOT",
                        Some(std::ffi::OsStr::new(&plugin_root)),
                    ),
                    (
                        "HERDR_PLUGIN_EVENT_JSON",
                        Some(std::ffi::OsStr::new("stale-event")),
                    ),
                    (
                        "HERDR_PLUGIN_CONTEXT_JSON",
                        Some(std::ffi::OsStr::new("stale-context")),
                    ),
                ],
            ),
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

// ── landing ──────────────────────────────────────────────────────────────────

/// `with_herdr_mode(true)` on the FIRST herdr launch lands on the Services tab
/// with the herdr selector row under the cursor and its detail pane descended
/// (the `↵` shape), checks already recomputed so the first paint is not empty,
/// and marks the landing done in `[herdr] first_landing_done` — once, forever.
/// Construction's recompute starts the herdr probe on its worker; under test no
/// prober is set (it would read the real registry), which stands in for a
/// probe that has not landed: the landing stays PENDING (parked on the last
/// row, list focus) and lands on the herdr row by label in the recompute that
/// adopts the probe. The `claude --version` probe stays `r`-gated:
/// construction must not block the first paint on a spawn.
#[test]
fn herdr_mode_lands_on_the_plugin_tab_with_the_herdr_row_selected() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app().with_herdr_mode(true);

    assert_eq!(
        app.tab,
        super::Tab::Services,
        "herdr mode opens on Services"
    );
    assert!(app.herdr_mode);
    assert!(
        app.services.land_on_herdr,
        "the landing intent is pending while herdr is unresolved"
    );
    assert_eq!(
        app.services.focus,
        super::ServicesFocus::List,
        "with no herdr resolved the landing does not descend into another row"
    );
    assert_eq!(
        app.services.herdr_options_cursor, 0,
        "the landed options cursor starts on the first row"
    );
    assert!(
        app.services.herdr.is_none() && !app.services.herdr_probe.running,
        "no prober under test: herdr stays unprobed, standing in for a probe \
         that has not landed"
    );
    assert!(
        app.services.cc_version.is_none(),
        "construction must not spawn `claude --version`; the probe stays `r`-gated"
    );
    let labels: Vec<&str> = app.services.checks.iter().map(|c| c.label).collect();
    assert_eq!(
        labels,
        vec!["shunt", "delegates", "plugin"],
        "construction recomputes the checks, so the first paint is not empty \
         (herdr does not render until its probe resolves)"
    );
    assert_eq!(
        app.services.cursor, 2,
        "the pending landing parks on the last row"
    );
    assert_eq!(
        app.services.selected_check().map(|c| c.label),
        Some("plugin"),
        "with no herdr resolved the parked cursor rests on the last row"
    );
    // Unprobed must read as unprobed, never as a missing binary.
    let plugin = &app.services.checks[2];
    assert_eq!(plugin.label, "plugin");
    assert!(
        plugin.detail.iter().any(|l| *l
            == format!(
                "claude: press {} to probe",
                crate::tui::render::prose::key("r")
            )),
        "the plugin row invites the `r` probe: {:?}",
        plugin.detail
    );
    assert!(
        !plugin.detail.iter().any(|l| l == "claude: not found"),
        "an unprobed version must not claim claude is missing: {:?}",
        plugin.detail
    );

    // The probe resolves (its drain adopts it and recomputes): the herdr row
    // inserts and the pending landing selects it by label.
    app.services.herdr = Some(Some(healthy_herdr_probe()));
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(app.services.checks.iter().any(|c| c.label == "herdr"));
    assert_eq!(
        app.services.selected_check().map(|c| c.label),
        Some("herdr"),
        "the pending landing re-lands on the herdr row once it appears"
    );
    assert_eq!(
        app.services.focus,
        super::ServicesFocus::Detail,
        "and descends into the herdr detail"
    );
    assert!(
        !app.services.land_on_herdr,
        "landing clears the pending intent"
    );

    // The landing slot itself: with a probe already cached, the pending
    // intent lands on the herdr row in the same recompute.
    let mut landed = bare_app();
    landed.services.land_on_herdr = true;
    landed.services.herdr = Some(Some(healthy_herdr_probe()));
    super::recompute_services_checks(&mut landed, false, super::ShuntRefresh::Keep);
    assert_eq!(
        landed.services.selected_check().map(|c| c.label),
        Some("herdr"),
        "a pending landing selects the herdr row once it renders"
    );

    // A user cursor move while the landing is still pending cancels it: the
    // user has taken over, so a later herdr resolve must not yank the cursor.
    let mut moved = bare_app();
    moved.tab = super::Tab::Services;
    moved.services.land_on_herdr = true;
    moved.services.herdr = Some(None);
    super::recompute_services_checks(&mut moved, false, super::ShuntRefresh::Keep);
    assert!(
        moved.services.land_on_herdr,
        "fixture control: still pending"
    );
    super::handle_key(&mut moved, crate::testutil::key(super::KeyCode::Up));
    assert!(
        !moved.services.land_on_herdr,
        "moving the cursor clears the pending landing"
    );
    moved.services.herdr = Some(Some(healthy_herdr_probe()));
    super::recompute_services_checks(&mut moved, false, super::ShuntRefresh::Keep);
    assert_ne!(
        moved.services.selected_check().map(|c| c.label),
        Some("herdr"),
        "a cancelled landing does not re-land when herdr later resolves"
    );

    // ↵ also clears the intent: the user has descended, so focus must not move.
    let mut descended = bare_app();
    descended.tab = super::Tab::Services;
    descended.services.land_on_herdr = true;
    descended.services.herdr = Some(None);
    super::recompute_services_checks(&mut descended, false, super::ShuntRefresh::Keep);
    super::handle_key(&mut descended, crate::testutil::key(super::KeyCode::Enter));
    assert!(
        !descended.services.land_on_herdr,
        "↵ clears the pending landing"
    );

    // A tab switch clears the intent too: leaving the tab means no later
    // recompute may yank the cursor back to herdr.
    let mut left = bare_app();
    left.tab = super::Tab::Services;
    left.services.land_on_herdr = true;
    left.services.herdr = Some(None);
    super::recompute_services_checks(&mut left, false, super::ShuntRefresh::Keep);
    super::switch_tab(&mut left, super::Tab::Overview);
    assert!(
        !left.services.land_on_herdr,
        "a tab switch clears the pending landing"
    );

    // `r` is still the only thing that probes the version.
    super::recompute_services_checks(&mut app, true, super::ShuntRefresh::Keep);
    assert!(
        app.services.cc_version.is_some(),
        "`r` runs the version probe"
    );

    // The landing is a one-time door: the marker it persists makes the next
    // launch open the home tab (pinned in the sibling test).
    let saved = std::fs::read_to_string(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("profiles.toml"),
    )
    .unwrap_or_default();
    assert!(
        saved.contains("first_landing_done = true"),
        "the first landing persists its marker: {saved}"
    );
    assert_eq!(
        app.last_reload_fp,
        crate::profile::reload_fingerprint(),
        "the first landing adopts its own marker write, so the first tick \
         does not re-read the config as an external change"
    );
}

/// A launch outside herdr mode opens the `home tab` (default `overview`) and
/// never touches the herdr landing marker.
#[test]
fn a_plain_app_lands_on_overview_with_the_first_row_selected() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = bare_app().with_herdr_mode(false);
    assert_eq!(
        app.tab,
        super::Tab::Overview,
        "a plain app opens the home tab (overview by default)"
    );
    assert!(!app.herdr_mode);
    assert_eq!(app.services.cursor, 0);
    assert!(
        app.services.checks.is_empty(),
        "no construction recompute outside the first herdr landing"
    );
    let saved = std::fs::read_to_string(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("profiles.toml"),
    )
    .unwrap_or_default();
    assert!(
        !saved.contains("first_landing_done"),
        "a plain launch never writes the herdr landing marker: {saved}"
    );
}

/// Two herdr launches, like two real runs: the first lands on Plugin with the
/// herdr detail open and persists `first_landing_done`; the second — a fresh
/// app loading the saved state — opens the `home tab` instead and skips the
/// eager probe the first landing paid for.
#[test]
fn the_herdr_landing_fires_once_then_later_launches_open_the_home_tab() {
    let _home = crate::testutil::HomeSandbox::new();
    let first = bare_app().with_herdr_mode(true);
    assert_eq!(
        first.tab,
        super::Tab::Services,
        "the first herdr launch lands on Plugin"
    );
    let saved = std::fs::read_to_string(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("profiles.toml"),
    )
    .unwrap_or_default();
    assert!(
        saved.contains("first_landing_done = true"),
        "the first landing writes its marker: {saved}"
    );

    let config = crate::profile::load_config().expect("reload");
    let second = App::new(config).with_herdr_mode(true);
    assert_eq!(
        second.tab,
        super::Tab::Overview,
        "the second herdr launch opens the home tab (overview by default)"
    );
    assert!(
        second.services.checks.is_empty(),
        "a later herdr launch skips the eager probe the first landing paid for"
    );
}

/// A plain launch honors the configured home tab: a top-level `home_tab = "plugin"`
/// in profiles.toml loads through the alias onto the Services tab, with no
/// herdr-mode flag and no eager probe.
#[test]
fn a_plain_launch_with_home_tab_set_lands_on_that_tab() {
    let _home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("profiles.toml"),
        "active_profile = \"acct\"\nprofiles = [\"acct\"]\n\
         home_tab = \"plugin\"\n",
    )
    .expect("write");
    let config = crate::profile::load_config().expect("load");
    let app = App::new(config).with_herdr_mode(false);
    assert_eq!(
        app.tab,
        super::Tab::Services,
        "the configured home tab decides a plain launch"
    );
    assert!(!app.herdr_mode);
    assert!(
        app.services.checks.is_empty(),
        "a plain launch runs no eager probe, whatever the home tab"
    );
    let saved = std::fs::read_to_string(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("profiles.toml"),
    )
    .unwrap_or_default();
    assert!(
        !saved.contains("first_landing_done"),
        "a plain launch never writes the herdr landing marker: {saved}"
    );
}

/// Space on the appearance band's `home tab` row cycles `overview` → `usage`
/// in tab order and persists, so the next launch opens the new tab.
#[test]
fn home_tab_cycles_from_the_config_appearance_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    let home_row = super::GLOBAL_CONFIG_ROWS.iter().copied().find(|row| {
        row.band() == "appearance"
            && !matches!(
                row,
                super::GlobalConfigRow::Theme
                    | super::GlobalConfigRow::ResetShape
                    | super::GlobalConfigRow::ClockNotation
            )
    });
    assert!(
        home_row.is_some(),
        "the appearance band holds the home tab row"
    );
    super::run_global_config_row(&mut app, home_row.expect("found"));
    let saved = std::fs::read_to_string(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("profiles.toml"),
    )
    .expect("read");
    assert!(
        saved.contains("home_tab = \"usage\""),
        "the cycle persists overview -> usage: {saved}"
    );
    let config = crate::profile::load_config().expect("reload");
    let relaunch = App::new(config).with_herdr_mode(false);
    assert_eq!(
        relaunch.tab,
        super::Tab::Usage,
        "the next launch opens the cycled home tab"
    );
}

// ── the login modal's paste door ─────────────────────────────────────────────

/// The login modal's inline code field, as `p` opened it.
fn paste_field(app: &App) -> super::InputState {
    app.login
        .as_ref()
        .and_then(|s| s.paste_field.clone())
        .unwrap_or_else(|| {
            panic!(
                "expected the code field open (login {}, modals {:?})",
                if app.login.is_some() { "live" } else { "gone" },
                app.modals
            )
        })
}

/// An app with an OAuth login in flight and its progress modal open, plus the
/// receiver the login's worker would hold. The clipboard escape goes to a sink,
/// never to the stdout the suite runs on.
fn app_with_open_login_modal() -> (
    App,
    std::sync::mpsc::Receiver<crate::oauth_login::ManualCode>,
) {
    let mut app = app_with(vec![]);
    app.tab = super::Tab::Setup;
    app.clipboard = |link| crate::platform::write_osc52(&mut std::io::sink(), link);
    app.login_generation = 1;
    let (session, rx) = paste_session("fresh", true, 1);
    app.login = Some(session);
    app.modals.push(super::Modal::Login);
    (app, rx)
}

/// `p` turns its own row into the code field on the session — the modal stack
/// is untouched — and esc restores the row with the field gone, so the next
/// `p` starts from an empty field. Only the esc after that collapses the modal.
#[test]
fn p_on_the_login_modal_opens_the_paste_field_and_esc_returns_to_it() {
    use super::{Modal, handle_key};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let (mut app, _rx) = app_with_open_login_modal();

    handle_key(&mut app, key(KeyCode::Char('p')));
    assert!(paste_field(&app).value.is_empty());
    assert!(
        matches!(app.modals.as_slice(), [Modal::Login]),
        "`p` pushes no modal, got {:?}",
        app.modals
    );
    for c in "abc".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    assert_eq!(paste_field(&app).value, "abc");

    handle_key(&mut app, key(KeyCode::Esc));
    let session = app
        .login
        .as_ref()
        .expect("esc on the field cancels nothing");
    assert!(
        session.paste_field.is_none(),
        "esc restores the `p  paste code` row"
    );
    assert!(
        matches!(app.modals.as_slice(), [Modal::Login]),
        "the modal stays, got {:?}",
        app.modals
    );
    assert_eq!(app.login_generation, 1);

    handle_key(&mut app, key(KeyCode::Char('p')));
    assert!(
        paste_field(&app).value.is_empty(),
        "the typed bytes did not survive esc"
    );
    assert!(app.toasts.is_empty(), "neither key toasts");

    handle_key(&mut app, key(KeyCode::Esc));
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(
        app.modals.is_empty(),
        "the esc after the field closed collapses the modal, got {:?}",
        app.modals
    );
    assert!(app.login.is_some(), "collapsing cancels nothing");
}

/// The field is a text input, so the login modal's own keys are data here (`c`,
/// `q`, `p` included — a paste arrives as one key event per character), and it
/// stops taking characters at `MANUAL_CODE_MAX`: the cap `parse` enforces is
/// applied at the door, so a runaway paste is never held.
#[test]
fn the_paste_field_takes_every_character_as_data_and_stops_at_the_cap() {
    use super::{Modal, handle_key};
    use crate::oauth_login::MANUAL_CODE_MAX;
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let (mut app, _rx) = app_with_open_login_modal();
    handle_key(&mut app, key(KeyCode::Char('p')));

    for c in "cqp#cqp".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    assert_eq!(
        paste_field(&app).value,
        "cqp#cqp",
        "every printable character is data in the field"
    );
    assert!(
        matches!(app.modals.as_slice(), [Modal::Login]),
        "`q` closed nothing, `p` opened nothing, got {:?}",
        app.modals
    );
    assert!(app.toasts.is_empty(), "`c` copied nothing");

    for _ in 0..MANUAL_CODE_MAX {
        handle_key(&mut app, key(KeyCode::Char('a')));
    }
    assert_eq!(
        paste_field(&app).value.len(),
        MANUAL_CODE_MAX,
        "characters past the cap are dropped at the door"
    );
    assert!(
        matches!(app.modals.as_slice(), [Modal::Login]),
        "the modal stays open"
    );
}

/// While the field is open the modal's action keys are data: `r`, `c` and `q`
/// land in the field, and none of their actions fires — no clipboard write, no
/// toast (a browser open toasts either way), no collapse. The fixture holds no
/// URL, so a leaked `r` could not spawn a real browser here; the field's value
/// is what proves it never reached that arm.
#[test]
fn r_c_and_q_are_data_while_the_field_is_open() {
    use super::{Modal, handle_key};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let (mut app, _rx) = app_with_open_login_modal();
    static HANDED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    app.clipboard = |link| {
        HANDED.lock().expect("handed").push(link.to_string());
        Ok(())
    };
    if let Some(s) = app.login.as_mut() {
        s.url = None;
    }
    handle_key(&mut app, key(KeyCode::Char('p')));

    for c in "rcq".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    assert_eq!(paste_field(&app).value, "rcq");
    assert!(
        HANDED.lock().expect("handed").is_empty(),
        "`c` reached the field, not the clipboard"
    );
    assert!(app.toasts.is_empty(), "no action fired");
    assert!(
        matches!(app.modals.as_slice(), [Modal::Login]),
        "`q` collapsed nothing, got {:?}",
        app.modals
    );
    assert!(app.login.is_some(), "the login keeps running");
}

/// A paste that fails the shape or state check is cleared, not kept: a
/// corrected paste appended to leftover bytes would pass the shape check and
/// burn the exchange. The field stays open, the toast carries the canned line
/// (shared with the CLI) and nothing reaches the worker or the session's door.
#[test]
fn a_bad_paste_clears_the_field_toasts_and_keeps_the_modal() {
    use super::{LoginMethod, LoginStage, Modal, ToastKind, handle_key};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let (mut app, rx) = app_with_open_login_modal();
    handle_key(&mut app, key(KeyCode::Char('p')));

    for (bad, toast_body) in [
        (
            "garbage",
            "invalid code; paste the code again including the #\n\
             cleared, paste the code again",
        ),
        (
            "abc#not-this-state",
            "state mismatch: code came from a different login\ncleared, paste the code again",
        ),
    ] {
        for c in bad.chars() {
            handle_key(&mut app, key(KeyCode::Char(c)));
        }
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(
            paste_field(&app).value.is_empty(),
            "{bad}: the bad paste is cleared and the field stays open"
        );
        assert!(
            matches!(app.modals.as_slice(), [Modal::Login]),
            "{bad}: the modal stays, got {:?}",
            app.modals
        );
        let toast = app
            .toasts
            .pop_back()
            .unwrap_or_else(|| panic!("{bad}: no toast"));
        assert_eq!(toast.kind, ToastKind::Danger);
        assert_eq!(toast.body, toast_body);
        assert!(rx.try_recv().is_err(), "{bad}: nothing reached the worker");
        let session = app.login.as_ref().expect("the login keeps running");
        assert_eq!(session.method, LoginMethod::Browser);
        assert_eq!(session.stage, LoginStage::WaitingBrowser);
    }
}

/// A good paste goes down the SESSION's own door — the one the running login's
/// worker reads, checked against that login's `state` — and the field closes.
/// The session is otherwise untouched: which door won is the worker's report
/// to make, because the browser callback may already have.
#[test]
fn a_good_paste_lands_on_the_sessions_paste_door() {
    use super::{LoginMethod, LoginStage, Modal, handle_key};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let (mut app, rx) = app_with_open_login_modal();
    handle_key(&mut app, key(KeyCode::Char('p')));

    for c in format!("the-code#{PASTE_STATE}").chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    handle_key(&mut app, key(KeyCode::Enter));

    let code = rx.try_recv().expect("the worker's receiver got the code");
    assert_eq!(code.as_str(), "the-code");
    assert!(rx.try_recv().is_err(), "exactly one send");
    let session = app.login.as_ref().expect("the login keeps running");
    assert!(
        session.paste_field.is_none(),
        "the field closes on a good send"
    );
    assert!(
        matches!(app.modals.as_slice(), [Modal::Login]),
        "the modal stays, got {:?}",
        app.modals
    );
    assert_eq!(
        session.method,
        LoginMethod::Browser,
        "the UI never names the door"
    );
    assert_eq!(session.stage, LoginStage::WaitingBrowser);
    assert_eq!(app.login_generation, 1, "no second login was minted");
    assert!(
        app.toasts.is_empty(),
        "a good paste says nothing; the worker's report will"
    );
}

/// The door rides the worker's `ExchangingCode` report and nothing else: a
/// `Manual` report flips the session's method, a `Browser` one leaves it, and
/// both move the stage.
#[test]
fn the_drain_takes_the_door_from_the_workers_report() {
    use super::{LoginEvent, LoginMethod, LoginStage, drain_login_events};
    let _home = crate::testutil::HomeSandbox::new();

    for (door, method_after) in [
        (LoginMethod::Browser, LoginMethod::Browser),
        (LoginMethod::Manual, LoginMethod::Manual),
    ] {
        let (mut app, _rx) = app_with_open_login_modal();
        app.login_event_tx
            .send((1, LoginEvent::Stage(LoginStage::ExchangingCode(door))))
            .unwrap();
        drain_login_events(&mut app);
        let session = app.login.as_ref().expect("session stays live");
        assert_eq!(session.method, method_after, "{door:?}");
        assert_eq!(session.stage, LoginStage::ExchangingCode(door), "{door:?}");

        // The verify bump keeps the door the exchange named.
        app.login_event_tx
            .send((1, LoginEvent::Stage(LoginStage::Verifying)))
            .unwrap();
        drain_login_events(&mut app);
        let session = app.login.as_ref().expect("session stays live");
        assert_eq!(session.method, method_after, "{door:?}");
        assert_eq!(session.stage, LoginStage::Verifying, "{door:?}");
    }
}

/// The worker→UI mapping behind the drain: each `oauth_login` milestone lands
/// as its own stage, the door it names included. The worker closure itself
/// binds a listener, so this seam is the one a test can cross.
#[test]
fn login_event_maps_each_worker_milestone_to_its_stage() {
    use super::{LoginEvent, LoginMethod, LoginStage, login_event};
    use crate::oauth_login::LoginProgress;
    for (progress, stage) in [
        (
            LoginProgress::ExchangingCode(LoginMethod::Manual),
            LoginStage::ExchangingCode(LoginMethod::Manual),
        ),
        (
            LoginProgress::ExchangingCode(LoginMethod::Browser),
            LoginStage::ExchangingCode(LoginMethod::Browser),
        ),
        (LoginProgress::Verifying, LoginStage::Verifying),
    ] {
        match login_event(progress) {
            LoginEvent::Stage(got) => assert_eq!(got, stage, "{progress:?}"),
            LoginEvent::Url(url) => panic!("{progress:?}: a milestone is never a url ({url})"),
        }
    }
}

/// The field never outlives its door. The drain closes it the moment a stage
/// bump closes the door (the browser callback won while a code was being
/// typed), and a submit that finds the door closed (the drain a tick behind)
/// closes it too, sending nothing.
#[test]
fn a_landed_door_closes_the_code_field() {
    use super::{LoginEvent, LoginMethod, LoginStage, Modal, drain_login_events, handle_key};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let (mut app, _rx) = app_with_open_login_modal();
    handle_key(&mut app, key(KeyCode::Char('p')));
    handle_key(&mut app, key(KeyCode::Char('a')));
    app.login_event_tx
        .send((
            1,
            LoginEvent::Stage(LoginStage::ExchangingCode(LoginMethod::Browser)),
        ))
        .unwrap();
    drain_login_events(&mut app);
    let session = app.login.as_ref().expect("session stays live");
    assert!(
        session.paste_field.is_none(),
        "the field goes with its door"
    );
    assert_eq!(
        session.stage,
        LoginStage::ExchangingCode(LoginMethod::Browser)
    );
    assert!(
        matches!(app.modals.as_slice(), [Modal::Login]),
        "the modal stays for the stage line, got {:?}",
        app.modals
    );

    let (mut app, rx) = app_with_open_login_modal();
    handle_key(&mut app, key(KeyCode::Char('p')));
    for c in format!("the-code#{PASTE_STATE}").chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    if let Some(s) = app.login.as_mut() {
        s.stage = LoginStage::ExchangingCode(LoginMethod::Browser);
    }
    handle_key(&mut app, key(KeyCode::Enter));
    assert!(rx.try_recv().is_err(), "nothing is sent into a closed door");
    let session = app.login.as_ref().expect("session stays live");
    assert!(session.paste_field.is_none(), "the submit closes the field");
    assert!(
        app.toasts.is_empty(),
        "and says nothing: the drain's stage line will"
    );
}

/// `c` hands the hosted link to the clipboard writer and says so; the writer
/// here records what it was handed, and the escape's bytes are pinned on a
/// writer in `platform`'s own tests.
#[test]
fn c_on_the_login_modal_sends_the_link_and_toasts() {
    use super::{Modal, ToastKind, handle_key};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let (mut app, _rx) = app_with_open_login_modal();
    static HANDED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    app.clipboard = |link| {
        HANDED.lock().expect("handed").push(link.to_string());
        Ok(())
    };

    handle_key(&mut app, key(KeyCode::Char('c')));
    assert_eq!(
        *HANDED.lock().expect("handed"),
        vec![format!(
            "https://hosted.example/authorize?state={PASTE_STATE}"
        )],
        "the hosted link, whole, and only it"
    );
    let toast = app.toasts.pop_back().expect("`c` toasts");
    assert_eq!(toast.kind, ToastKind::Info);
    assert_eq!(toast.body, "link copied to clipboard");
    assert!(
        matches!(app.modals.as_slice(), [Modal::Login]),
        "`c` opens no modal, got {:?}",
        app.modals
    );

    // A writer that cannot reach the terminal is named, not swallowed.
    app.clipboard = |_| Err(std::io::Error::other("stdout is closed"));
    handle_key(&mut app, key(KeyCode::Char('c')));
    let toast = app.toasts.pop_back().expect("a failed `c` toasts too");
    assert_eq!(toast.kind, ToastKind::Danger);
    assert_eq!(
        toast.body,
        "couldn't copy the link to clipboard\nstdout is closed"
    );
}

/// The paste door is what `c` and `p` answer to. A session without one (the
/// console login's shape) and a session already exchanging a code (a door
/// landed) leave both keys inert: no field, no toast.
#[test]
fn c_and_p_are_inert_without_an_open_paste_door() {
    use super::{LoginMethod, LoginStage, Modal, handle_key};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut console = app_with(vec![]);
    console.login_generation = 1;
    let mut session = login_session("qwen", false, 1);
    session.url = Some("https://console.example/login".to_string());
    console.login = Some(session);
    console.modals.push(Modal::Login);

    let (mut landed, _rx) = app_with_open_login_modal();
    if let Some(s) = landed.login.as_mut() {
        s.stage = LoginStage::ExchangingCode(LoginMethod::Browser);
    }

    for (label, app) in [("console", &mut console), ("landed", &mut landed)] {
        for c in ['c', 'p'] {
            handle_key(app, key(KeyCode::Char(c)));
            assert!(
                matches!(app.modals.as_slice(), [Modal::Login]),
                "{label}: `{c}` pushed nothing, got {:?}",
                app.modals
            );
            assert!(app.toasts.is_empty(), "{label}: `{c}` toasted nothing");
        }
        let session = app.login.as_ref().expect("the login keeps running");
        assert!(
            session.paste_field.is_none(),
            "{label}: `p` opened no field"
        );
    }
}

/// The `+ new` form runs name to create with one login row and nothing beside
/// it: the exact runtime sequence, like the OAuth account's tail above.
#[test]
fn config_rows_on_the_new_form_carry_one_login_row() {
    use super::{ConfigRow, build_draft_new, config_rows};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![]);
    app.profile_cursor = 0; // == profile_count() → the `+ new` form
    app.config_draft = Some(build_draft_new());
    app.unsaved_live_login = false;

    let rows = config_rows(&app);
    assert_eq!(
        rows,
        [
            ConfigRow::Name,
            ConfigRow::BaseUrl,
            ConfigRow::Model,
            ConfigRow::Login,
            ConfigRow::Create,
        ],
        "{rows:?}"
    );
}

// ── the global `c` arm: Overview filter, Tokens cache toggle ─────────────────

/// `c` is the Tokens tab's cache-counting toggle and the Overview's harness
/// filter. The global arm claims it for the Overview alone; on every other tab
/// the key falls through to the per-tab dispatch, so the Tokens binding the
/// footer and help modal advertise still runs.
#[test]
fn c_on_the_tokens_tab_flips_count_cache() {
    use super::{HarnessFilter, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("a"),
    )]);
    app.tab = Tab::Tokens;
    let before = app.config().state.count_cache;

    handle_key(&mut app, crate::testutil::key(KeyCode::Char('c')));
    assert_eq!(
        app.config().state.count_cache,
        !before,
        "the Tokens `c` binding must still reach `toggle_count_cache`"
    );
    assert_eq!(
        app.harness_filter,
        HarnessFilter::All,
        "the harness filter is the Overview's alone"
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Char('c')));
    assert_eq!(
        app.config().state.count_cache,
        before,
        "a second `c` flips it back"
    );
}

/// On the Overview `c` cycles the filter both → claude → codex → both and
/// leaves the Tokens flag alone; on a tab with no `c` binding it is a no-op.
#[test]
fn c_on_the_overview_cycles_the_harness_filter_and_leaves_count_cache_alone() {
    use super::{HarnessFilter, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![crate::testutil::blank_profile(
        &crate::profile::ProfileName::from("a"),
    )]);
    app.tab = Tab::Overview;
    let count_cache = app.config().state.count_cache;

    let mut seen = Vec::new();
    for _ in 0..3 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Char('c')));
        seen.push(app.harness_filter);
    }
    assert_eq!(
        seen,
        [
            HarnessFilter::Claude,
            HarnessFilter::Codex,
            HarnessFilter::All
        ]
    );
    assert_eq!(
        app.config().state.count_cache,
        count_cache,
        "the Overview's `c` never touches the Tokens flag"
    );

    app.tab = Tab::Usage;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('c')));
    assert_eq!(
        app.harness_filter,
        HarnessFilter::All,
        "no `c` binding on Usage"
    );
    assert_eq!(app.config().state.count_cache, count_cache);
}

// ── Overview selection across harnesses ─────────────────────────────────────

#[test]
fn leaving_overview_does_not_restore_an_old_claude_selection() {
    use super::{Tab, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("a")),
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("b")),
    ]);
    handle_key(&mut app, crate::testutil::key(KeyCode::Right));
    assert_eq!(app.tab, Tab::Usage);
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(app.profile_cursor, 1);
    handle_key(&mut app, crate::testutil::key(KeyCode::Right));
    assert_eq!(
        app.profile_cursor, 1,
        "the Usage selection survives leaving that tab"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Left));
    assert_eq!(
        app.profile_cursor, 1,
        "returning to Usage keeps the selected account"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Left));
    assert_eq!(
        app.overview_cursor, 1,
        "Overview follows the account selected in Usage"
    );
}

#[test]
fn removing_the_selected_codex_account_targets_the_visible_claude_fallback() {
    use super::{MainItemKind, Modal, handle_key, poll_codex_rows};
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1"]);
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("a")),
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("b")),
    ]);
    app.tab = Tab::Overview;
    app.overview_cursor = 2;
    assert!(matches!(
        app.current_main_item(),
        Some(MainItemKind::Codex(0))
    ));
    assert_eq!(
        app.profile_cursor, 0,
        "a remains the previous Claude selection"
    );
    crate::testutil::write_codex_roster(&[]);
    app.last_codex_rows_refresh = None;
    poll_codex_rows(&mut app);
    assert!(matches!(
        app.current_main_item(),
        Some(MainItemKind::Profile(1))
    ));
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('a')));
    assert!(
        matches!(app.modals.last(), Some(Modal::ActionMenu(m)) if m.context.as_deref() == Some("b"))
    );
    assert_eq!(app.profile_cursor, 1);
}

#[test]
fn filtering_a_codex_selection_back_to_claude_targets_the_visible_account() {
    use super::{MainItemKind, Modal, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1"]);
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("a")),
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("b")),
    ]);
    app.tab = Tab::Overview;
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(matches!(
        app.current_main_item(),
        Some(MainItemKind::Codex(0))
    ));
    assert_eq!(
        app.profile_cursor, 1,
        "b was the last selected Claude account"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('c')));
    assert!(matches!(
        app.current_main_item(),
        Some(MainItemKind::Profile(0))
    ));
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('a')));
    assert!(
        matches!(app.modals.last(), Some(Modal::ActionMenu(m)) if m.context.as_deref() == Some("a"))
    );
    assert_eq!(
        app.profile_cursor, 0,
        "account actions follow the visible caret"
    );
}

#[test]
fn codex_rows_can_be_selected_without_acting_on_a_claude_account() {
    use super::{HarnessFilter, KeyEvent, KeyModifiers, MainItemKind, Modal, handle_key};
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1", "cx2", "cx3"]);
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("a")),
        crate::testutil::blank_profile(&crate::profile::ProfileName::from("b")),
    ]);
    app.tab = Tab::Overview;
    assert_eq!(app.codex_rows.len(), 3, "fixture loaded every codex row");
    assert_eq!(app.main_items().len(), 5, "all five accounts are reachable");

    for _ in 0..4 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
    assert_eq!(app.overview_cursor, 4);
    assert_eq!(
        app.current_main_item().map(|item| format!("{item:?}")),
        Some("Codex(2)".to_string())
    );
    let before = app.config().state.profiles.clone();
    handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(
        app.config().state.profiles,
        before,
        "codex does not reorder claude"
    );
    assert!(app.modals.is_empty(), "codex does not open a claude switch");
    assert!(
        app.toasts.is_empty(),
        "an inert action raises no false filter toast"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('a')));
    assert!(matches!(app.modals.last(), Some(Modal::ActionMenu(m)) if m.scoped_len == 0));
    app.modals.clear();

    handle_key(&mut app, crate::testutil::key(KeyCode::Char('c')));
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('c')));
    assert_eq!(app.harness_filter, HarnessFilter::Codex);
    assert_eq!(
        app.overview_cursor, 0,
        "a hidden selection lands on the first visible row"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(
        app.overview_cursor, 2,
        "codex-only navigation wraps to the last row"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        app.overview_cursor, 0,
        "codex-only navigation wraps to the first row"
    );
    assert_eq!(
        app.current_main_item().map(|item| format!("{item:?}")),
        Some("Codex(0)".to_string())
    );
    assert!(app.toasts.is_empty());
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('c')));
    assert_eq!(app.harness_filter, HarnessFilter::All);
    assert_eq!(
        app.overview_cursor, 2,
        "the same codex row remains selected in all view"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(
        app.overview_cursor, 1,
        "claude cursor reaches the second row"
    );
    assert!(matches!(
        app.current_main_item(),
        Some(MainItemKind::Profile(1))
    ));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(matches!(app.modals.last(), Some(Modal::Confirm(_))));
}

// ── the day-list collision warning ───────────────────────────

fn app_with_chain(profiles: Vec<crate::profile::Profile>) -> App {
    use crate::profile::{AppConfig, AppState};
    let names: Vec<crate::profile::ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    App::new(AppConfig {
        state: AppState {
            profiles: names.clone(),
            fallback_chain: names,
            ..AppState::default()
        },
        profiles,
    })
}

/// Every weekday, so a fixture reads the same whatever day the suite runs on.
fn all_weekdays() -> Vec<chrono::Weekday> {
    use chrono::Weekday::*;
    vec![Mon, Tue, Wed, Thu, Fri, Sat, Sun]
}

/// Two notices are two gate keys, not one: a passed-over lister arriving while
/// a collision is still up must raise its own toast and leave the collision's
/// alone. Holding one string would either repaint both or swallow the second.
#[test]
fn a_second_day_notice_does_not_repaint_the_first() {
    use super::warn_day_claim_notices;
    use crate::profile::{Profile, ProfileName};
    let _home = crate::testutil::HomeSandbox::new();

    let mut a = Profile::new("work".to_string(), None, None);
    a.preferred_days = all_weekdays();
    let mut b = Profile::new("personal".to_string(), None, None);
    b.preferred_days = all_weekdays();
    let mut app = app_with_chain(vec![a, b]);

    warn_day_claim_notices(&mut app);
    assert_eq!(app.toasts.len(), 1, "the collision says itself once");
    let collision = app.toasts[0].body.clone();

    // A third account, off the chain, names the same days: its list is passed
    // over while the two above still collide.
    {
        let mut cfg = app.config();
        let mut spare = Profile::new("spare".to_string(), None, None);
        spare.preferred_days = all_weekdays();
        cfg.state.profiles.push(ProfileName::from("spare"));
        cfg.profiles.push(spare);
    }
    warn_day_claim_notices(&mut app);
    assert_eq!(
        app.toasts.len(),
        2,
        "the new notice is raised, got {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );
    assert_eq!(
        app.toasts[0].body, collision,
        "and the collision is not re-raised"
    );
    assert!(
        app.toasts[1].body.contains("'spare'"),
        "the second names the passed-over account: {}",
        app.toasts[1].body
    );

    warn_day_claim_notices(&mut app);
    assert_eq!(app.toasts.len(), 2, "the next tick repaints neither");
}

/// The chain pass re-derives the claim every tick, so the warning has to be
/// edge-triggered: once when the collision appears, silent while it stands,
/// again once the claimants change.
#[test]
fn the_day_collision_warning_fires_on_the_edge_only() {
    use super::warn_day_claim_notices;
    use crate::profile::{Profile, ProfileName};
    let _home = crate::testutil::HomeSandbox::new();

    let mut a = Profile::new("work".to_string(), None, None);
    a.preferred_days = all_weekdays();
    let mut b = Profile::new("personal".to_string(), None, None);
    b.preferred_days = all_weekdays();
    let mut app = app_with_chain(vec![a, b]);

    warn_day_claim_notices(&mut app);
    assert_eq!(app.toasts.len(), 1, "the collision says itself once");
    assert!(
        app.toasts[0].body.contains("2 accounts claim"),
        "got {:?}",
        app.toasts[0].body
    );

    warn_day_claim_notices(&mut app);
    assert_eq!(app.toasts.len(), 1, "the next tick repaints nothing");

    {
        let mut cfg = app.config();
        if let Some(p) = cfg.find_mut(&ProfileName::from("personal")) {
            p.preferred_days.clear();
        }
    }
    warn_day_claim_notices(&mut app);
    assert_eq!(app.toasts.len(), 1, "clearing the collision says nothing");

    {
        let mut cfg = app.config();
        if let Some(p) = cfg.find_mut(&ProfileName::from("personal")) {
            p.preferred_days = all_weekdays();
        }
    }
    warn_day_claim_notices(&mut app);
    assert_eq!(app.toasts.len(), 2, "a collision re-introduced warns again");
}

// ── the Fallback card's `preferred days` row ─────────────────────────────────

/// The row's index in `FALLBACK_ROWS`, read off the list itself so a future
/// inserted row can never silently re-point these tests.
fn preferred_days_row() -> usize {
    super::FALLBACK_ROWS
        .iter()
        .position(|r| *r == super::FallbackRow::PreferredDays)
        .expect("the row exists")
}

/// A disk-backed app, one chain member, cursor on the `preferred days` row. A
/// member with a list is saved whole to disk as well as memory, since the row's
/// save reads the profile it edits off disk.
fn preferred_days_app(days: Vec<chrono::Weekday>) -> App {
    let mut p = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    p.preferred_days = days;
    if !p.preferred_days.is_empty() {
        crate::profile::save_profile(&p).expect("save the member");
    }
    let mut app = app_with_unlinked_profiles(vec![p]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = preferred_days_row();
    app
}

/// Member `a`'s list on disk ([`disk_days`]).
fn on_disk_days() -> Vec<chrono::Weekday> {
    disk_days("a")
}

/// Write a list to the member's own file, so an unchanged file afterwards is a
/// real claim rather than a missing one.
fn seed_disk_days(app: &App, days: Vec<chrono::Weekday>) {
    let mut cfg = app.config();
    crate::actions::edit_profile_preferred_days(
        &mut cfg,
        &crate::profile::ProfileName::from("a"),
        |_| days,
    )
    .expect("seed the member's list");
}

#[test]
fn fallback_preferred_days_space_walks_the_preset_ladder_and_persists() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let mut app = preferred_days_app(Vec::new());

    let ladder: [(Vec<chrono::Weekday>, &str); 4] = [
        (vec![Mon, Tue, Wed, Thu, Fri], "weekdays"),
        (vec![Sat, Sun], "weekends"),
        (super::WEEKDAYS_ALL.to_vec(), "every day"),
        (Vec::new(), "never"),
    ];
    for (expected, rung) in ladder {
        super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(on_disk_days(), expected, "space steps onto `{rung}`");
    }
}

// A hand-written set matches no rung, so space lands on the ladder's first: a
// set has no order, so no rung sits above it the way the weekly threshold
// stepper finds the next preset above a custom percent.
#[test]
fn fallback_preferred_days_space_steps_a_custom_set_to_never() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let app = preferred_days_app(vec![Wed, Fri]);
    seed_disk_days(&app, vec![Wed, Fri]);
    assert_eq!(
        on_disk_days(),
        vec![Wed, Fri],
        "precondition: the custom set"
    );

    let mut app = app;
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert!(on_disk_days().is_empty(), "a custom set steps to `never`");
}

/// Space from `every day` onto `a`'s remembered custom stop, or onto `never`
/// when there is none, each step read back off disk.
fn step_days(app: &mut App, want: &[Vec<chrono::Weekday>], label: &str) {
    for (i, days) in want.iter().enumerate() {
        super::handle_key(app, key(KeyCode::Char(' ')));
        assert_eq!(&on_disk_days(), days, "{label}: press {}", i + 1);
    }
}

/// A custom list is one more stop in the cycle while the card stays open:
/// `never → weekdays → weekends → every day → the list → never`, each press
/// saved at once, so stepping past the list and around again restores it.
#[test]
fn space_keeps_a_custom_day_list_in_the_cycle_while_the_card_is_open() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let mut app = preferred_days_app(vec![Wed, Fri]);
    assert_eq!(
        on_disk_days(),
        vec![Wed, Fri],
        "precondition: the custom list"
    );

    step_days(
        &mut app,
        &[
            Vec::new(),
            vec![Mon, Tue, Wed, Thu, Fri],
            vec![Sat, Sun],
            super::WEEKDAYS_ALL.to_vec(),
            vec![Wed, Fri],
            Vec::new(),
        ],
        "around the cycle and back",
    );
}

/// The remembered stop belongs to the open card: leaving it for the chain list
/// forgets the stop, so the next trip round the cycle ends on `never`. The
/// first leg proves the stop was held before the card was left.
#[test]
fn leaving_the_card_forgets_the_custom_stop() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let presets_then_never = [
        vec![Mon, Tue, Wed, Thu, Fri],
        vec![Sat, Sun],
        super::WEEKDAYS_ALL.to_vec(),
        Vec::new(),
    ];
    let mut app = preferred_days_app(vec![Wed, Fri]);
    step_days(
        &mut app,
        &[
            Vec::new(),
            vec![Mon, Tue, Wed, Thu, Fri],
            vec![Sat, Sun],
            super::WEEKDAYS_ALL.to_vec(),
            vec![Wed, Fri],
            Vec::new(),
        ],
        "the stop is held while the card is open",
    );

    super::handle_key(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.fallback_focus,
        super::FallbackFocus::Chain,
        "esc left the card"
    );
    super::handle_key(&mut app, key(KeyCode::Enter));
    for _ in 0..preferred_days_row() {
        super::handle_key(&mut app, key(KeyCode::Down));
    }
    step_days(&mut app, &presets_then_never, "after leaving the card");

    // A tab switch away and back leaves the card too.
    seed_disk_days(&app, vec![Wed, Fri]);
    step_days(&mut app, &[Vec::new()], "stepped off the custom list again");
    super::handle_key(&mut app, key(KeyCode::Right));
    assert_ne!(app.tab, Tab::Fallback, "→ left the tab");
    super::handle_key(&mut app, key(KeyCode::Left));
    assert_eq!(app.tab, Tab::Fallback, "← came back");
    super::handle_key(&mut app, key(KeyCode::Enter));
    for _ in 0..preferred_days_row() {
        super::handle_key(&mut app, key(KeyCode::Down));
    }
    step_days(&mut app, &presets_then_never, "after a tab round trip");
}

/// A reload that takes the card off its member forgets the member's stop too:
/// with no edit open the card stays on the slot, so it shows whoever took it,
/// and coming back to the member is a new card. The first leg proves the stop
/// was held before the reload.
#[test]
fn a_reload_that_moves_the_card_off_its_member_forgets_the_custom_stop() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let mut app = two_member_app();
    seed_disk_days(&app, vec![Wed, Fri]);
    step_days(
        &mut app,
        &[
            Vec::new(),
            vec![Mon, Tue, Wed, Thu, Fri],
            vec![Sat, Sun],
            super::WEEKDAYS_ALL.to_vec(),
            vec![Wed, Fri],
            Vec::new(),
        ],
        "the stop is held while the card is open",
    );

    reload_with_chain(&mut app, &["b", "a"]);
    assert_eq!(app.chain_cursor, 0, "the card stayed on the slot `b` took");
    // The fingerprint reads mtimes alone, so the way back needs one of its own.
    let mut state = crate::profile::load_app_state().expect("load the state");
    state.fallback_chain = vec!["a".into(), "b".into()];
    crate::profile::save_app_state(&state).expect("rewrite the state");
    let path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("profiles.toml");
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(3_000_000),
    );
    assert!(app.reload_if_state_changed(), "the reload back ran");
    assert_eq!(
        app.chain_cursor, 0,
        "precondition: the card is on `a` again"
    );

    step_days(
        &mut app,
        &[
            vec![Mon, Tue, Wed, Thu, Fri],
            vec![Sat, Sun],
            super::WEEKDAYS_ALL.to_vec(),
            Vec::new(),
        ],
        "after the card moved off `a`",
    );
}

/// The picker's caret, `None` while it is closed.
fn picker_caret(app: &App) -> Option<usize> {
    match app.fallback_edit.as_ref().map(|e| &e.state) {
        Some(super::CardEdit::Days(picker)) => Some(picker.cursor),
        _ => None,
    }
}

// No draft: each space writes the member's toggled list at once, and the
// picker stays open for the next one.
#[test]
fn each_space_in_the_day_picker_saves_the_toggled_list() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let mut app = preferred_days_app(vec![Mon, Fri]);

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert_eq!(picker_caret(&app), Some(0), "the caret starts on Monday");

    super::handle_key(&mut app, key(KeyCode::Right));
    super::handle_key(&mut app, key(KeyCode::Right));
    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        on_disk_days(),
        vec![Mon, Wed, Fri],
        "→ → space saved Wednesday on and left the rest alone"
    );
    assert_eq!(picker_caret(&app), Some(2), "the picker stays open");

    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(on_disk_days(), vec![Mon, Fri], "space again saved it off");
}

/// A toggle is computed from the list read off disk under the lock, so a day
/// another writer saved since the last reload survives it.
#[test]
fn a_picker_toggle_keeps_a_day_another_writer_saved() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let mut app = preferred_days_app(Vec::new());
    let name = crate::profile::ProfileName::from("a");
    let mut other = crate::profile::load_profile(&name).expect("load a");
    other.preferred_days = vec![Wed];
    crate::profile::save_profile(&other).expect("the other writer's save");

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    super::handle_key(&mut app, key(KeyCode::Char(' ')));

    assert_eq!(
        on_disk_days(),
        vec![Mon, Wed],
        "Monday joined the list on disk"
    );
}

/// A press answers the list the card shows: with another writer's list on
/// disk, space on a shown-on day saves it off and a preset steps from the shown
/// rung, never inverting what the operator saw.
#[test]
fn a_press_follows_the_shown_list_when_another_writer_raced_it() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let name = crate::profile::ProfileName::from("a");
    let race = |days: Vec<chrono::Weekday>| {
        let mut other = crate::profile::load_profile(&name).expect("load a");
        other.preferred_days = days;
        crate::profile::save_profile(&other).expect("the other writer's save");
    };

    let mut app = preferred_days_app(vec![Mon]);
    race(vec![Fri]);
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(on_disk_days(), vec![Fri], "Monday went off, as shown");

    let mut app = preferred_days_app(vec![Mon, Tue, Wed, Thu, Fri]);
    race(vec![Sat, Sun]);
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        on_disk_days(),
        vec![Sat, Sun],
        "`weekdays` stepped to `weekends`, the rung after the shown one"
    );
}

/// Space at rest on a chain entry with no account behind it does nothing, the
/// same answer ⏎ gives there.
#[test]
fn space_on_a_chain_entry_with_no_account_does_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = preferred_days_app(Vec::new());
    app.config()
        .state
        .fallback_chain
        .insert(0, crate::profile::ProfileName::from("ghost"));
    app.chain_cursor = 0;

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    assert!(app.toasts.is_empty(), "no toast for `ghost`");
    assert!(
        !crate::profile::profile_subpath(
            &crate::profile::ProfileName::from("ghost"),
            "config.toml"
        )
        .expect("path")
        .exists(),
        "nothing written for `ghost`"
    );
}

/// The row's save re-reads the profile under the lock, so a field another
/// writer changed since the last reload is not rewound by the day save.
#[test]
fn a_day_save_keeps_a_field_another_writer_changed() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let mut app = preferred_days_app(Vec::new());
    let name = crate::profile::ProfileName::from("a");
    let mut other = crate::profile::load_profile(&name).expect("load a");
    other.fallback_threshold = Some(55.0);
    crate::profile::save_profile(&other).expect("the other writer's save");

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    let after = crate::profile::load_profile(&name).expect("reload a");
    assert_eq!(after.preferred_days, vec![Mon, Tue, Wed, Thu, Fri]);
    assert_eq!(
        after.fallback_threshold,
        Some(55.0),
        "the threshold edit survived"
    );
}

/// A chain entry with no account behind it draws no `preferred days` row, so ⏎
/// there opens no picker to claim the keys invisibly.
#[test]
fn enter_on_a_chain_entry_with_no_account_opens_no_picker() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = preferred_days_app(Vec::new());
    app.config()
        .state
        .fallback_chain
        .insert(0, crate::profile::ProfileName::from("ghost"));
    app.chain_cursor = 0;

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));

    assert!(picker_caret(&app).is_none(), "no picker opened on `ghost`");
}

/// An account deleted on disk since the last reload is refused by name, and the
/// save never recreates its directory.
#[test]
fn a_day_save_refuses_an_account_deleted_since_the_reload() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = preferred_days_app(Vec::new());
    let name = crate::profile::ProfileName::from("a");
    let dir = crate::profile::profile_subpath(&name, "config.toml")
        .expect("config path")
        .parent()
        .expect("profile dir")
        .to_path_buf();
    assert!(
        !dir.exists(),
        "precondition: the fixture holds no dir for a"
    );
    let mut state = crate::profile::load_app_state().expect("load the roster");
    state.profiles.retain(|n| *n != name);
    crate::profile::save_app_state(&state).expect("drop a from the roster");

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    assert_eq!(app.toasts.len(), 1, "one toast");
    assert!(matches!(app.toasts[0].kind, super::ToastKind::Danger));
    assert_eq!(
        app.toasts[0].body,
        "preferred days update failed\nprofile not found"
    );
    assert!(
        !dir.exists(),
        "the save recreated the deleted account's dir"
    );
}

#[test]
fn opening_and_leaving_the_day_picker_writes_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let mut app = preferred_days_app(vec![Mon, Fri]);
    seed_disk_days(&app, vec![Mon, Fri]);
    let path =
        crate::profile::profile_subpath(&crate::profile::ProfileName::from("a"), "config.toml")
            .expect("config path");
    let before = std::fs::read(&path).expect("read the seeded file");

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    super::handle_key(&mut app, key(KeyCode::Right));
    super::handle_key(&mut app, key(KeyCode::Esc));

    assert!(picker_caret(&app).is_none(), "esc left the picker");
    assert_eq!(
        std::fs::read(&path).expect("read back"),
        before,
        "walking the caret and leaving wrote nothing"
    );
}

#[test]
fn the_day_picker_caret_wraps_both_ways() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = preferred_days_app(Vec::new());

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    super::handle_key(&mut app, key(KeyCode::Left));
    assert_eq!(picker_caret(&app), Some(6), "← wraps past Monday to Sunday");
    super::handle_key(&mut app, key(KeyCode::Right));
    assert_eq!(picker_caret(&app), Some(0), "→ wraps past Sunday to Monday");
    assert_eq!(app.tab, Tab::Fallback, "neither arrow switched tabs");
}

// While descended the picker binds neither `?` nor `x`, so both keep their
// global senses mid-pick: `?` opens the help modal and `x` dismisses the
// front toast, the picker's own claims-nothing warning included, and the
// picker stays open through both. `a`, tab and shift-tab stay claimed.
#[test]
fn the_day_picker_passes_help_and_dismiss_and_claims_actions_and_tab() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = dead_member_app(Vec::new());

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.toasts.iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![super::ToastKind::Warning],
        "precondition: the save raised the claims-nothing warning"
    );

    super::handle_key(&mut app, key(KeyCode::Char('?')));
    assert!(
        matches!(app.modals.as_slice(), [super::Modal::Help]),
        "`?` opened the help modal"
    );
    assert_eq!(
        picker_caret(&app),
        Some(0),
        "the picker stayed open under it"
    );
    super::handle_key(&mut app, key(KeyCode::Esc));
    assert!(app.modals.is_empty(), "esc closed the help modal");
    assert_eq!(picker_caret(&app), Some(0), "and left the picker open");

    super::handle_key(&mut app, key(KeyCode::Char('x')));
    assert!(app.toasts.is_empty(), "`x` dismissed the warning");
    assert_eq!(picker_caret(&app), Some(0), "the picker stayed open");

    for code in [KeyCode::Char('a'), KeyCode::Tab, KeyCode::BackTab] {
        super::handle_key(&mut app, key(code));
        assert!(app.modals.is_empty(), "{code:?} opened no modal");
        assert_eq!(app.tab, Tab::Fallback, "{code:?} switched no tab");
        assert_eq!(picker_caret(&app), Some(0), "{code:?} left the picker open");
    }
    assert_eq!(
        on_disk_days(),
        vec![chrono::Weekday::Mon],
        "only the one space wrote"
    );
}

/// A dead account with a list on the Fallback card, cursor on the day row.
fn dead_member_app(days: Vec<chrono::Weekday>) -> App {
    let mut p = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    p.disabled = true;
    p.preferred_days = days;
    if !p.preferred_days.is_empty() {
        crate::profile::save_profile(&p).expect("save the member");
    }
    let mut app = app_with_unlinked_profiles(vec![p]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = preferred_days_row();
    app
}

// An empty list claims nothing on any account, so clearing a dead account's
// list is a plain save with nothing to warn about.
#[test]
fn a_dead_account_stepping_to_never_raises_no_toast() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = dead_member_app(super::WEEKDAYS_ALL.to_vec());

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    assert!(on_disk_days().is_empty(), "`every day` stepped to `never`");
    assert!(
        app.toasts.is_empty(),
        "no warning, got {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );
}

// The picker warns about a dead account once per descend, not once per
// toggle: the first save that leaves a list raises it, the toggles after it
// stay quiet, and a fresh descend says it again.
#[test]
fn a_dead_account_warns_once_per_day_picker_descend() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let warning = "saved, but this list claims nothing: the account is disabled\n\
                   the chain decides those days without this account";
    let mut app = dead_member_app(Vec::new());

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    super::handle_key(&mut app, key(KeyCode::Right));
    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(on_disk_days(), vec![Mon, Tue], "both toggles saved");
    assert_eq!(
        app.toasts
            .iter()
            .map(|t| t.body.as_str())
            .collect::<Vec<_>>(),
        vec![warning],
        "one warning for the whole descend"
    );
    assert_eq!(app.toasts[0].kind, super::ToastKind::Warning);

    super::handle_key(&mut app, key(KeyCode::Enter));
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(on_disk_days(), vec![Tue], "the new descend saved too");
    assert_eq!(
        app.toasts
            .iter()
            .map(|t| t.body.as_str())
            .collect::<Vec<_>>(),
        vec![warning, warning],
        "a fresh descend warns again"
    );
}

// A list on an account the walk skips is saved and then explained, with the
// blocker's own wording: refusing the save would hide a state `is_home_on`
// already handles, and saving it in silence would leave a row that reads set
// and does nothing.
#[test]
fn a_dead_accounts_day_list_saves_with_the_claim_nothing_warning() {
    let _home = crate::testutil::HomeSandbox::new();
    use chrono::Weekday::*;
    let mut app = dead_member_app(Vec::new());

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    assert_eq!(
        on_disk_days(),
        vec![Mon, Tue, Wed, Thu, Fri],
        "the list is saved anyway"
    );
    assert_eq!(app.toasts.len(), 1, "one toast, not a stack");
    assert!(matches!(app.toasts[0].kind, super::ToastKind::Warning));
    assert_eq!(
        app.toasts[0].body,
        "saved, but this list claims nothing: the account is disabled\n\
         the chain decides those days without this account",
        "the warning names the blocker and what the list cannot do"
    );
}

/// `name`'s list as it sits in its own `config.toml` — read back through the
/// loader, so a change that never reached disk fails here.
fn disk_days(name: &str) -> Vec<chrono::Weekday> {
    crate::profile::load_profile(&crate::profile::ProfileName::from(name))
        .expect("reload the member")
        .preferred_days
}

/// Chain `[a, b]` on disk and in memory, both members saved, the cursor on
/// `a`'s `preferred days` row.
fn two_member_app() -> App {
    use crate::profile::ProfileName;
    let a = crate::testutil::blank_profile(&ProfileName::from("a"));
    let b = crate::testutil::blank_profile(&ProfileName::from("b"));
    crate::profile::save_profile(&a).expect("seed a");
    crate::profile::save_profile(&b).expect("seed b");
    let mut app = app_with_unlinked_profiles(vec![a, b]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = preferred_days_row();
    app
}

/// Rewrite the chain on disk the way another writer would (the REST chain
/// route, a second TUI), then run the tick's reload over it.
fn reload_with_chain(app: &mut App, chain: &[&str]) {
    reload_with_state(app, |state| {
        state.fallback_chain = chain
            .iter()
            .map(|n| crate::profile::ProfileName::from(*n))
            .collect();
    });
}

/// Rewrite `profiles.toml` through `edit` the way another writer would, then
/// run the tick's reload over it.
fn reload_with_state(app: &mut App, edit: impl FnOnce(&mut AppState)) {
    let mut state = crate::profile::load_app_state().expect("load the state");
    edit(&mut state);
    crate::profile::save_app_state(&state).expect("rewrite the state");
    // A distinct mtime, so the fingerprint moves however fast the write lands.
    let path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("profiles.toml");
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000),
    );
    assert!(app.reload_if_state_changed(), "the reload branch ran");
}

/// The picker is pinned to the member it opened on, by name: a reload that
/// reorders the chain under it moves the cursor with that member, and the next
/// toggle lands on it rather than on whoever took its old slot.
#[test]
fn the_day_picker_follows_its_member_through_a_reorder_reload() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = two_member_app();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    reload_with_chain(&mut app, &["b", "a"]);
    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    super::handle_key(&mut app, key(KeyCode::Enter));

    assert_eq!(
        (disk_days("a"), disk_days("b")),
        (vec![chrono::Weekday::Mon], Vec::new()),
        "the toggle lands on the member the picker opened on"
    );
    assert_eq!(
        app.chain_cursor, 1,
        "the cursor followed `a` to its new slot"
    );
    assert!(picker_caret(&app).is_none(), "⏎ left the picker");
}

/// A reload that drops the member from the chain closes its picker and hands
/// focus back to the chain list: there is no member left for a toggle to write
/// to, and the next key must not land on the member that moved into the slot.
#[test]
fn the_day_picker_closes_when_a_reload_drops_its_member() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = two_member_app();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert!(
        picker_caret(&app).is_some(),
        "precondition: the picker is open on `a`"
    );
    reload_with_chain(&mut app, &["b"]);

    assert!(
        picker_caret(&app).is_none(),
        "the picker closed with its member gone"
    );
    assert_eq!(
        app.fallback_focus,
        super::FallbackFocus::Chain,
        "focus went back to the chain list"
    );
    super::handle_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        (disk_days("a"), disk_days("b")),
        (Vec::new(), Vec::new()),
        "closing wrote nothing, and a space meant for the picker reached no member"
    );
}

/// `name`'s own `config.toml` whole: a write meant for another member that
/// lands here changes it.
fn member_file(name: &str) -> String {
    let path =
        crate::profile::profile_subpath(&crate::profile::ProfileName::from(name), "config.toml")
            .expect("config path");
    std::fs::read_to_string(path).expect("read the member file")
}

/// On `a`'s card, open the typed field on `row` with ⏎, clear its seed and type
/// `value`, leaving the field open.
fn type_into_card_field(app: &mut App, row: super::FallbackRow, value: &str) {
    app.fallback_detail_cursor = super::FALLBACK_ROWS
        .iter()
        .position(|r| *r == row)
        .expect("the row exists");
    super::handle_key(app, key(KeyCode::Enter));
    for _ in 0..8 {
        super::handle_key(app, key(KeyCode::Backspace));
    }
    for c in value.chars() {
        super::handle_key(app, key(KeyCode::Char(c)));
    }
}

/// A typed field opened on `a`, a reload that puts `b` in `a`'s slot, then ⏎:
/// the commit lands on `a` and `b`'s file never moves.
fn commit_across_a_reorder_reload(app: &mut App, row: super::FallbackRow, value: &str) {
    let b_before = member_file("b");
    type_into_card_field(app, row, value);
    reload_with_chain(app, &["b", "a"]);
    super::handle_key(app, key(KeyCode::Enter));
    assert_eq!(member_file("b"), b_before, "`b`'s file is byte-identical");
}

#[test]
fn the_rotate_at_field_commits_to_its_member_through_a_reorder_reload() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = two_member_app();

    commit_across_a_reorder_reload(&mut app, super::FallbackRow::Threshold, "42");

    assert_eq!(
        crate::profile::load_profile(&crate::profile::ProfileName::from("a"))
            .expect("reload a")
            .fallback_threshold,
        Some(42.0),
        "`a` carries the typed threshold"
    );
}

#[test]
fn the_weekly_at_field_commits_to_its_member_through_a_reorder_reload() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = two_member_app();

    commit_across_a_reorder_reload(&mut app, super::FallbackRow::WeeklyAt, "90");

    assert_eq!(
        crate::profile::load_profile(&crate::profile::ProfileName::from("a"))
            .expect("reload a")
            .weekly_threshold,
        Some(90.0),
        "`a` carries the typed weekly line"
    );
}

/// The costly one: a dollar ceiling written on the wrong account is money the
/// operator never agreed to spend there.
#[test]
fn the_max_spend_field_commits_to_its_member_through_a_reorder_reload() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = two_member_app();
    let mut state = crate::profile::load_app_state().expect("load the state");
    state.spend_budget_switching = true;
    crate::profile::save_app_state(&state).expect("arm the spend budget");
    app.config().state.spend_budget_switching = true;

    commit_across_a_reorder_reload(&mut app, super::FallbackRow::MaxSpend, "25");

    assert_eq!(
        crate::profile::load_profile(&crate::profile::ProfileName::from("a"))
            .expect("reload a")
            .max_auto_spend,
        Some(25.0),
        "`a` carries the typed ceiling"
    );
}

/// The armed remove is pinned like a typed field: the confirming ⏎ after a
/// reorder reload removes the member it armed on, and only that one.
#[test]
fn the_armed_remove_removes_its_member_through_a_reorder_reload() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = two_member_app();
    let b_before = member_file("b");
    app.fallback_detail_cursor = super::FALLBACK_ROWS
        .iter()
        .position(|r| *r == super::FallbackRow::Remove)
        .expect("the row exists");

    super::handle_key(&mut app, key(KeyCode::Enter));
    reload_with_chain(&mut app, &["b", "a"]);
    super::handle_key(&mut app, key(KeyCode::Enter));

    assert_eq!(
        crate::profile::load_app_state()
            .expect("load the state")
            .fallback_chain,
        vec![crate::profile::ProfileName::from("b")],
        "`a` left the chain and `b` stayed"
    );
    assert_eq!(member_file("b"), b_before, "`b`'s file is byte-identical");
}

/// Moving off an armed remove disarms it: ↓ then ↑ lands back on `remove`
/// unarmed, so the next ⏎ arms it afresh and removes nothing.
#[test]
fn moving_off_an_armed_remove_disarms_it() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = two_member_app();
    app.fallback_detail_cursor = super::FALLBACK_ROWS
        .iter()
        .position(|r| *r == super::FallbackRow::Remove)
        .expect("the row exists");

    super::handle_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        card_edit_row(&app),
        Some(super::FallbackRow::Remove),
        "precondition: the first ⏎ armed it"
    );
    super::handle_key(&mut app, key(KeyCode::Down));
    super::handle_key(&mut app, key(KeyCode::Up));
    super::handle_key(&mut app, key(KeyCode::Enter));

    assert_eq!(
        crate::profile::load_app_state()
            .expect("load the state")
            .fallback_chain,
        vec![
            crate::profile::ProfileName::from("a"),
            crate::profile::ProfileName::from("b")
        ],
        "one ⏎ after moving away removed nothing"
    );
    assert_eq!(
        card_edit_row(&app),
        Some(super::FallbackRow::Remove),
        "that ⏎ armed it afresh"
    );
}

/// The write reads the pin, never the cursor. No key moves `chain_cursor` while
/// an edit is open and a reload re-pins it, so the test moves it onto `b` by
/// hand, standing in for any path that leaves it elsewhere. Every edit is
/// checked before any assertion fires.
#[test]
fn a_card_edit_writes_its_member_wherever_the_cursor_points() {
    use super::FallbackRow;
    use crate::profile::{Profile, ProfileName};
    type Landed = fn(&Profile, &[ProfileName]) -> bool;
    let cases: [(FallbackRow, &str, Landed); 4] = [
        (FallbackRow::Threshold, "42", |a, _| {
            a.fallback_threshold == Some(42.0)
        }),
        (FallbackRow::WeeklyAt, "90", |a, _| {
            a.weekly_threshold == Some(90.0)
        }),
        (FallbackRow::MaxSpend, "25", |a, _| {
            a.max_auto_spend == Some(25.0)
        }),
        (FallbackRow::Remove, "", |_, chain| {
            chain == [ProfileName::from("b")]
        }),
    ];
    let mut wrong = Vec::new();
    for (row, value, landed) in cases {
        let _home = crate::testutil::HomeSandbox::new();
        let mut app = two_member_app();
        app.config().state.spend_budget_switching = true;
        let b_before = member_file("b");
        if row == FallbackRow::Remove {
            app.fallback_detail_cursor = super::FALLBACK_ROWS
                .iter()
                .position(|r| *r == row)
                .expect("the row exists");
            super::handle_key(&mut app, key(KeyCode::Enter));
        } else {
            type_into_card_field(&mut app, row, value);
        }

        app.chain_cursor = 1;
        super::handle_key(&mut app, key(KeyCode::Enter));

        let a = crate::profile::load_profile(&ProfileName::from("a")).expect("reload a");
        let chain = crate::profile::load_app_state()
            .expect("load the state")
            .fallback_chain;
        if !landed(&a, &chain) || member_file("b") != b_before {
            wrong.push(format!("{row:?}: the write missed `a` or touched `b`"));
        }
    }
    assert!(
        wrong.is_empty(),
        "writes that followed the cursor:\n{}",
        wrong.join("\n")
    );
}

/// An account that leaves the roster while its name stays on the chain (only a
/// hand-edited `profiles.toml` gets there) closes whatever edit its card held
/// and hands focus back to the chain list, for every edit the card can hold.
/// Every edit is checked before any assertion fires.
#[test]
fn a_card_edit_closes_when_a_reload_drops_its_account_from_the_roster() {
    use super::FallbackRow;
    let mut wrong = Vec::new();
    for row in [
        FallbackRow::Threshold,
        FallbackRow::WeeklyAt,
        FallbackRow::MaxSpend,
        FallbackRow::PreferredDays,
        FallbackRow::Remove,
    ] {
        let _home = crate::testutil::HomeSandbox::new();
        let mut app = two_member_app();
        app.config().state.spend_budget_switching = true;
        app.fallback_detail_cursor = super::FALLBACK_ROWS
            .iter()
            .position(|r| *r == row)
            .expect("the row exists");
        super::handle_key(&mut app, key(KeyCode::Enter));

        reload_with_state(&mut app, |state| {
            state.profiles.retain(|n| n.as_str() != "a")
        });
        assert_eq!(
            app.config().state.fallback_chain,
            vec![
                crate::profile::ProfileName::from("a"),
                crate::profile::ProfileName::from("b")
            ],
            "{row:?}: precondition, `a` is still on the chain"
        );

        let focus = app.fallback_focus;
        // → switches tabs only once no edit owns the arrows.
        super::handle_key(&mut app, key(KeyCode::Right));
        if focus != super::FallbackFocus::Chain || app.tab != Tab::Fallback.next() {
            wrong.push(format!(
                "{row:?}: focus {focus:?}, → left the tab at {:?}",
                app.tab
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "edits left open on a member gone from the roster:\n{}",
        wrong.join("\n")
    );
}

/// Every way out of the picker the contract names: ⏎, esc and q leave the mode
/// on the same row; ↑/↓ leave the row and the mode together. None of them
/// ascends out of the card or writes.
#[test]
fn enter_esc_q_up_and_down_each_leave_the_day_picker() {
    let _home = crate::testutil::HomeSandbox::new();
    let row = preferred_days_row();
    let exits = [
        (KeyCode::Enter, row),
        (KeyCode::Esc, row),
        (KeyCode::Char('q'), row),
        (KeyCode::Up, row - 1),
        (KeyCode::Down, row + 1),
    ];
    for (code, lands_on) in exits {
        let mut app = preferred_days_app(Vec::new());
        super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
        assert!(
            picker_caret(&app).is_some(),
            "{code:?}: precondition, the picker is open"
        );

        super::handle_key(&mut app, key(code));

        assert!(picker_caret(&app).is_none(), "{code:?} leaves the picker");
        assert_eq!(
            app.fallback_focus,
            super::FallbackFocus::Detail,
            "{code:?} stays on the card"
        );
        assert_eq!(
            app.fallback_detail_cursor, lands_on,
            "{code:?} leaves the row cursor here"
        );
        assert!(on_disk_days().is_empty(), "{code:?} writes nothing");
    }
}

/// A save that fails leaves the in-memory list on what the disk still holds and
/// names the failure. Unix-only: it forces the failure by dropping write on the
/// member's own directory, the posture the repo's other save-failure probes take.
#[cfg(unix)]
#[test]
fn a_failed_day_list_save_keeps_memory_on_the_disk_value_and_names_the_error() {
    use std::os::unix::fs::PermissionsExt;
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("a");
    let mut app = preferred_days_app(Vec::new());
    seed_disk_days(&app, Vec::new());
    let dir = crate::profile::profile_dir(&name).expect("profile dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
        .expect("chmod the member dir read-only");

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    // Restore before any assertion so a failure still lets the sandbox clean up.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("restore the member dir");
    assert_eq!(
        app.config().find(&name).map(|p| p.preferred_days.clone()),
        Some(Vec::new()),
        "memory keeps the list the disk still holds"
    );
    assert!(on_disk_days().is_empty(), "nothing reached the disk");
    assert_eq!(app.toasts.len(), 1, "one toast");
    assert_eq!(app.toasts[0].kind, super::ToastKind::Danger);
    assert_eq!(
        app.toasts[0].body,
        "preferred days update failed\nfailed to write config.toml"
    );
}

// ── Note editor ───────────────────────────────────────────────────────────────

fn notes_app(profiles: Vec<crate::profile::Profile>) -> App {
    use crate::profile::{AppConfig, AppState};
    App::new(AppConfig {
        state: AppState::default(),
        profiles,
    })
}

/// `n` on the usage tab opens the account's note editor; an empty roster keeps
/// the empty state's `n to create one` promise; every other tab keeps the
/// global new-account flow.
#[test]
fn n_on_usage_opens_the_note_editor() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let _home = crate::testutil::HomeSandbox::new();

    // With an account, n opens the note editor for the selected account.
    let mut app = notes_app(vec![mini_profile("alice", None)]);
    app.tab = Tab::Usage;
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    assert!(
        app.note_editor.is_some(),
        "usage tab: n opens the note editor"
    );
    assert_eq!(app.note_editor.as_ref().unwrap().profile.as_str(), "alice");
    assert!(app.config_draft.is_none(), "new-account never started");

    // An empty roster keeps the empty state's `n to create one` promise.
    let mut app = notes_app(vec![]);
    app.tab = Tab::Usage;
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    assert!(app.note_editor.is_none(), "no account, no editor");
    assert!(
        app.config_draft.is_some(),
        "the empty state keeps n = new account"
    );

    // Every other tab keeps the global new-account flow.
    let mut app = notes_app(vec![mini_profile("alice", None)]);
    app.tab = Tab::Overview;
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    assert!(app.note_editor.is_none());
    assert!(app.config_draft.is_some());
}

/// ⏎ saves the draft into the profile dir and toasts; esc discards the draft.
#[test]
fn enter_saves_the_note_and_esc_discards_it() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let _home = crate::testutil::HomeSandbox::new();
    let profile = mini_profile("alice", None);
    crate::profile::save_profile(&profile).unwrap();
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec!["alice".into()],
        ..Default::default()
    })
    .unwrap();
    let mut app = notes_app(vec![profile]);
    app.tab = Tab::Usage;
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    assert!(app.note_editor.is_some(), "editor open");
    for ch in "hello".chars() {
        super::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        );
    }
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    );
    for ch in "world".chars() {
        super::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        );
    }
    super::handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let name = crate::profile::ProfileName::from("alice");
    assert_eq!(
        crate::profile_notes::load_note(&name).as_deref(),
        Some("hello\nworld"),
        "the draft landed on disk verbatim"
    );
    assert_eq!(app.note_text.as_deref(), Some("hello\nworld"));
    assert!(app.note_editor.is_none(), "a save closes the editor");
    assert_eq!(app.toasts[0].body, "note saved");

    // Reopen: seeded from the saved text.
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    assert_eq!(app.note_editor.as_ref().unwrap().buf.text(), "hello\nworld");

    // esc discards: the typed extra stays out of the stored note.
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );
    super::handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.note_editor.is_none());
    assert_eq!(
        crate::profile_notes::load_note(&name).as_deref(),
        Some("hello\nworld"),
        "esc never wrote"
    );
}

/// ⌃j splits the draft, backspace at a line start joins it, ⌃w deletes the
/// previous word.
#[test]
fn the_note_editor_keymap_edits_the_draft() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = notes_app(vec![mini_profile("alice", None)]);
    app.tab = Tab::Usage;
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    let press = |app: &mut App, code| {
        super::handle_key(app, KeyEvent::new(code, KeyModifiers::NONE));
    };
    for ch in "foo bar baz".chars() {
        press(&mut app, KeyCode::Char(ch));
    }
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
    );
    assert_eq!(draft(&app), "foo bar ", "⌃w removes the last word");
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    );
    assert_eq!(draft(&app), "foo bar \n", "⌃j splits at the caret");
    for ch in "ab".chars() {
        press(&mut app, KeyCode::Char(ch));
    }
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    assert_eq!(draft(&app), "foo bar ", "backspace joins at a line start");
}

/// A save for an account the roster dropped fails and keeps the editor open —
/// the draft is the only copy.
#[test]
fn a_failed_save_keeps_the_editor_open() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = notes_app(vec![mini_profile("alice", None)]);
    app.tab = Tab::Usage;
    super::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    for ch in "boo".chars() {
        super::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        );
    }
    super::handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        app.note_editor.is_some(),
        "the draft survives a failed save"
    );
    assert!(
        app.toasts[0].body.starts_with("couldn't save the note"),
        "the failure names itself, got {:?}",
        app.toasts[0].body
    );
}

fn draft(app: &App) -> String {
    app.note_editor.as_ref().unwrap().buf.text()
}

/// ↓ on the usage tab re-reads the note: the `notes:` row must never show the
/// previous account's note under the newly selected header.
#[test]
fn stepping_the_cursor_reloads_the_note() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let _home = crate::testutil::HomeSandbox::new();
    let alice = mini_profile("alice", None);
    let bob = mini_profile("bob", None);
    crate::profile::save_profile(&alice).unwrap();
    crate::profile::save_profile(&bob).unwrap();
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec!["alice".into(), "bob".into()],
        ..Default::default()
    })
    .unwrap();
    let alice_name = crate::profile::ProfileName::from("alice");
    let bob_name = crate::profile::ProfileName::from("bob");
    crate::profile_notes::save_note(&alice_name, "A note").unwrap();
    crate::profile_notes::save_note(&bob_name, "B note").unwrap();

    let mut app = notes_app(vec![alice, bob]);
    app.tab = Tab::Usage;
    assert_eq!(app.note_text.as_deref(), Some("A note"));
    super::handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        app.note_text.as_deref(),
        Some("B note"),
        "a cursor step re-reads the note for the new selection"
    );
}

// ── shunt card: the control surface ─────────────────────────────────────────

use crate::daemon::gateway::{Answerer, GatewaySlot, GatewayState};

fn shunt_slot(state: GatewayState) -> GatewaySlot {
    GatewaySlot {
        state,
        config: None,
        binary: None,
        port: None,
        pid: None,
        version: None,
        answerer: None,
        floor: "0.48.0".to_string(),
        restarts: 0,
        last_exit: None,
        reason: None,
        since: None,
    }
}

fn adopted_record(
    home: &crate::testutil::HomeSandbox,
    disabled: bool,
) -> crate::gateway::GatewayRecord {
    let config = home.home().join("etc").join("shunt.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "[server]\n").unwrap();
    let mut record = crate::gateway::GatewayRecord::new(config).unwrap();
    record.disabled = disabled;
    record
}

fn moved_file(from: &str, to: &str) -> crate::gateway::MovedFile {
    crate::gateway::MovedFile {
        from: std::path::PathBuf::from(from),
        to: std::path::PathBuf::from(to),
    }
}

/// `f adopt config` is offered only for a found TOML config: a YAML find keeps
/// its `bind  not read (yaml)` line and offers nothing.
#[test]
fn the_shunt_card_offers_adopt_only_for_a_toml_find() {
    let _home = crate::testutil::HomeSandbox::new();
    let toml = super::StandaloneShunt {
        found: Some(std::path::PathBuf::from("/cfg/shunt.toml")),
        unread_bind: None,
        unread_config: None,
        unread_env: false,
        answer: None,
    };
    let card = super::shunt_card(
        &shunt_slot(GatewayState::Absent),
        false,
        false,
        Some(&toml),
        None,
        None,
        None,
        None,
    );
    assert_eq!(
        card.detail.last().map(String::as_str),
        Some("f  adopt config"),
        "a TOML find gets the adopt line: {:?}",
        card.detail
    );
    assert!(
        matches!(card.fix, Some(super::ServiceFix::AdoptConfig(_))),
        "the list-focus fix is the adopt"
    );
    assert_eq!(card.shunt_focus.len(), 1, "one focusable: the adopt fix");

    let yaml = super::StandaloneShunt {
        found: Some(std::path::PathBuf::from("/cfg/shunt.yaml")),
        unread_bind: Some(super::UnreadBind::Yaml),
        unread_config: Some(super::UnreadConfig::Yaml),
        unread_env: false,
        answer: None,
    };
    let card = super::shunt_card(
        &shunt_slot(GatewayState::Absent),
        false,
        false,
        Some(&yaml),
        None,
        None,
        None,
        None,
    );
    assert!(card.fix.is_none(), "a YAML find gets no adopt");
    assert!(card.shunt_focus.is_empty());
    assert!(
        !card.detail.iter().any(|l| l.starts_with("f  ")),
        "no fix line on a YAML find: {:?}",
        card.detail
    );

    // Any unread bind — a YAML find, an unreadable or unparsed config, a
    // refused value — offers no adopt, not just the YAML case: the adopt's
    // probe would only ever fail. A refused value is the case whose config
    // clauth did read.
    let refused = super::StandaloneShunt {
        found: Some(std::path::PathBuf::from("/cfg/shunt.toml")),
        unread_bind: Some(super::UnreadBind::Value("${SHUNT_BIND}".to_string())),
        unread_config: None,
        unread_env: false,
        answer: None,
    };
    let card = super::shunt_card(
        &shunt_slot(GatewayState::Absent),
        false,
        false,
        Some(&refused),
        None,
        None,
        None,
        None,
    );
    assert!(
        card.fix.is_none(),
        "a refused bind gets no adopt: {:?}",
        card.detail
    );
    assert!(
        !card.detail.iter().any(|l| l.starts_with("f  ")),
        "no fix line on a refused bind: {:?}",
        card.detail
    );
}

/// The adopt refuses a config that does not parse on its own, before any
/// bind is read: the card's readout can be older than the file's breakage.
/// The held daemon lock with no env record makes the env read refuse, so only
/// the adopt's own parse step can produce the parse error.
#[test]
fn the_adopt_refuses_an_unparsed_config_before_reading_any_bind() {
    let home = crate::testutil::HomeSandbox::new();
    let config = home.home().join("shunt.toml");
    std::fs::write(&config, "a = 1\n[server\n").unwrap();
    let _held = crate::daemon::hold_daemon_lock();
    match super::run_adopt(&config) {
        super::ShuntActionOutcome::AdoptFailed { error } => {
            assert_eq!(error, "the shunt config does not parse as TOML (line 2)")
        }
        other => panic!("expected the parse refusal, got {other:?}"),
    }
    assert!(
        crate::gateway::GatewayRecord::load().unwrap().is_none(),
        "a refused adopt writes no record"
    );
}

/// A card action's error keeps its cause chain: an adopt whose config does not
/// exist names the path and, under it, why it could not be resolved. The
/// cause is the OS's own message, read here independently of the code under
/// test.
#[test]
fn a_failed_adopt_keeps_the_cause_under_its_context() {
    let home = crate::testutil::HomeSandbox::new();
    let config = home.home().join("etc").join("missing.toml");
    let cause = std::fs::canonicalize(&config).expect_err("no such file");
    match super::run_adopt(&config) {
        super::ShuntActionOutcome::AdoptFailed { error } => assert_eq!(
            error,
            format!(
                "cannot resolve the shunt config {}: {cause}",
                config.display()
            )
        ),
        other => panic!("expected the read failure, got {other:?}"),
    }
}

/// The card's `refused` and `pool` lines keep an error's cause chain too: a
/// plan whose env file cannot be read, a pool read whose token file cannot.
#[test]
fn the_plan_and_pool_lines_keep_the_cause_under_its_context() {
    let home = crate::testutil::HomeSandbox::new();
    let env_file = home.home().join("etc").join("missing.env");
    let mut record = crate::testutil::write_adopted_record(&home, false, "[server]\n");
    record.env_file = Some(env_file.clone());
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let cause = std::fs::read(&env_file).expect_err("no such file");
    assert_eq!(
        super::move_plan_probe(),
        super::MovePlanOutcome::Refused(format!(
            "failed to read env file {}: {cause}",
            env_file.display()
        ))
    );

    let mut record = crate::testutil::write_adopted_record(&home, false, "[server]\n");
    record.env_file = None;
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    // A directory where the token file belongs: the read fails with an io
    // error under the path's context.
    let token = home.home().join(".clauth").join("gateway-admin-token");
    std::fs::create_dir_all(&token).unwrap();
    let cause = std::fs::read_to_string(&token).expect_err("a directory");
    match super::pool_probe() {
        super::PoolOutcome::Error(error) => assert_eq!(
            error,
            format!("failed to read {}: {cause}", token.display())
        ),
        other => panic!("expected the token read failure, got {other:?}"),
    }
}

/// An env `SHUNT_SERVER__BIND` hides a YAML, unreadable or unparsed find from
/// the bind line, but the adopt would still fail on the config itself: the card offers
/// no adopt and a `config: not read (…)` line says why. Built through the
/// readout, so the producer and the card's gate are pinned together.
#[test]
fn an_env_bind_does_not_offer_adopt_for_an_unreadable_config() {
    let home = crate::testutil::HomeSandbox::new();
    let silent = |_| Ok(crate::gateway::Health::NotShunt { status: 404 });
    let yaml = std::path::PathBuf::from("/cfg/shunt.yaml");
    let dir = home.home().join("a-dir-not-a-file");
    std::fs::create_dir_all(&dir).unwrap();
    let unparsed = home.home().join("shunt.toml");
    std::fs::write(&unparsed, "[server\n").unwrap();
    for (what, standalone, want) in [
        (
            "a YAML find",
            super::standalone_readout_from(
                Err(crate::gateway::YamlConfig { path: yaml }.into()),
                Some("127.0.0.1:4200"),
                silent,
            ),
            vec![
                "state: not adopted".to_string(),
                "found: /cfg/shunt.yaml".to_string(),
                "config: not read (yaml)".to_string(),
            ],
        ),
        (
            "an unreadable find",
            super::standalone_readout_from(Ok(Some(dir.clone())), Some("127.0.0.1:4200"), silent),
            vec![
                "state: not adopted".to_string(),
                format!("found: {}", dir.display()),
                "config: not read (unreadable)".to_string(),
            ],
        ),
        (
            "an unparseable TOML find",
            super::standalone_readout_from(
                Ok(Some(unparsed.clone())),
                Some("127.0.0.1:4200"),
                silent,
            ),
            vec![
                "state: not adopted".to_string(),
                format!("found: {}", unparsed.display()),
                "config: not read (does not parse)".to_string(),
            ],
        ),
        (
            "a refused env bind over an unparseable TOML find",
            super::standalone_readout_from(Ok(Some(unparsed.clone())), Some("nope"), silent),
            vec![
                "state: not adopted".to_string(),
                format!("found: {}", unparsed.display()),
                "bind: not read (SHUNT_SERVER__BIND=nope)".to_string(),
                "config: not read (does not parse)".to_string(),
            ],
        ),
        (
            "a refused env bind over a YAML find",
            super::standalone_readout_from(
                Err(crate::gateway::YamlConfig {
                    path: std::path::PathBuf::from("/cfg/shunt.yaml"),
                }
                .into()),
                Some("nope"),
                silent,
            ),
            vec![
                "state: not adopted".to_string(),
                "found: /cfg/shunt.yaml".to_string(),
                "bind: not read (SHUNT_SERVER__BIND=nope)".to_string(),
                "config: not read (yaml)".to_string(),
            ],
        ),
    ] {
        assert_eq!(
            super::shunt_check(&shunt_slot(GatewayState::Absent), false, Some(&standalone)).detail,
            want,
            "{what}: the config line names what the bind line does not",
        );
        let card = super::shunt_card(
            &shunt_slot(GatewayState::Absent),
            false,
            false,
            Some(&standalone),
            None,
            None,
            None,
            None,
        );
        assert!(
            card.fix.is_none(),
            "{what} gets no adopt: {:?}",
            card.detail
        );
        assert!(card.shunt_focus.is_empty(), "{what}: nothing to focus");
        assert!(
            !card.detail.iter().any(|l| l.starts_with("f  ")),
            "{what}: no fix line: {:?}",
            card.detail
        );
    }
}

/// A `shunt.toml` that links onto a YAML file reads as TOML, but the adopt
/// resolves the link and refuses the YAML target: the readout reads it as
/// the YAML it is, so the card offers no adopt.
#[cfg(unix)]
#[test]
fn a_toml_name_linked_onto_yaml_reads_as_yaml() {
    let home = crate::testutil::HomeSandbox::new();
    let silent = |_| Ok(crate::gateway::Health::NotShunt { status: 404 });
    let target = home.home().join("real.yaml");
    std::fs::write(&target, "server:\n  bind: 127.0.0.1:4600\n").unwrap();
    let link = home.home().join("shunt.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let standalone = super::standalone_readout_from(Ok(Some(link.clone())), None, silent);
    assert_eq!(
        (standalone.unread_bind.clone(), standalone.unread_config),
        (
            Some(super::UnreadBind::Yaml),
            Some(super::UnreadConfig::Yaml)
        ),
    );
    let with_env =
        super::standalone_readout_from(Ok(Some(link.clone())), Some("127.0.0.1:4200"), silent);
    let card = super::shunt_card(
        &shunt_slot(GatewayState::Absent),
        false,
        false,
        Some(&with_env),
        None,
        None,
        None,
        None,
    );
    assert_eq!(
        card.detail,
        vec![
            "state: not adopted".to_string(),
            format!("found: {}", link.display()),
            "config: not read (yaml)".to_string(),
        ],
    );
    assert!(card.fix.is_none(), "no adopt for a link onto YAML");
}

/// A daemon holding the singleton with no env record leaves the bind its
/// adopt would read unknown: the readout probes the config's own bind, the
/// card says why in the approved copy, and offers no adopt. A read env bind
/// moves the probe, as before.
#[test]
fn an_unrecorded_daemon_env_offers_no_adopt() {
    use std::cell::RefCell;
    let home = crate::testutil::HomeSandbox::new();
    let probed = RefCell::new(Vec::new());
    let silent = |addr| {
        probed.borrow_mut().push(addr);
        Ok(crate::gateway::Health::NotShunt { status: 404 })
    };
    let config = home.home().join("shunt.toml");
    std::fs::write(&config, "[server]\nbind = \"127.0.0.1:4900\"\n").unwrap();
    let read = super::standalone_readout_with(
        Ok(Some(config.clone())),
        Ok(Some("127.0.0.1:4800".to_string())),
        silent,
    );
    assert!(!read.unread_env, "a read env is not unread");
    let standalone = super::standalone_readout_with(
        Ok(Some(config.clone())),
        Err(anyhow::anyhow!(
            "the running daemon recorded no environment"
        )),
        silent,
    );
    assert_eq!(
        *probed.borrow(),
        vec![
            "127.0.0.1:4800".parse::<std::net::SocketAddr>().unwrap(),
            "127.0.0.1:4900".parse().unwrap(),
        ],
        "the env bind when read, else the config's own"
    );
    assert_eq!(
        standalone,
        super::StandaloneShunt {
            found: Some(config.clone()),
            unread_bind: None,
            unread_config: None,
            unread_env: true,
            answer: None,
        }
    );
    let card = super::shunt_card(
        &shunt_slot(GatewayState::Absent),
        false,
        false,
        Some(&standalone),
        None,
        None,
        None,
        None,
    );
    assert_eq!(
        card.detail,
        vec![
            "state: not adopted".to_string(),
            format!("found: {}", config.display()),
            "the running daemon recorded no environment; restart it".to_string(),
        ],
    );
    assert!(card.fix.is_none(), "no adopt while the env is unknown");
    assert!(card.shunt_focus.is_empty());
}

/// The card's action section for an adopted record: a blank line, the `enabled`
/// row, the admin fix (by need), the pool note, then the MOVE PLAN.
#[test]
fn the_shunt_card_builds_the_enabled_admin_and_pool_section() {
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, false);
    let mut slot = shunt_slot(GatewayState::Healthy);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    slot.version = Some("0.49.1".to_string());

    // No admin step owed, no pool note, no move due: fields, blank, enabled.
    let card = super::shunt_card(&slot, true, false, None, Some(&record), None, None, None);
    assert_eq!(
        card.detail,
        vec![
            "binary: shunt".to_string(),
            format!("config: {}", record.config().display()),
            "version: 0.49.1".to_string(),
            "state: healthy".to_string(),
            "restarts: 0".to_string(),
            String::new(),
            "enabled".to_string(),
        ],
        "the adopted-healthy card"
    );
    assert!(
        matches!(
            card.shunt_focus.first(),
            Some(super::ShuntFocus::Enabled { .. })
        ),
        "the enabled row walks first"
    );

    // A write-key need offers `add admin key`; a table need offers `add admin
    // table`; Neither offers nothing.
    let key_card = super::shunt_card(
        &slot,
        true,
        false,
        None,
        Some(&record),
        Some(crate::gateway::AdminNeed::WriteKey),
        None,
        None,
    );
    assert!(
        key_card.detail.iter().any(|l| l == "f  add admin key"),
        "a write-key need names the key fix: {:?}",
        key_card.detail
    );
    let table_card = super::shunt_card(
        &slot,
        true,
        false,
        None,
        Some(&record),
        Some(crate::gateway::AdminNeed::AdminTable),
        None,
        None,
    );
    assert!(
        table_card.detail.iter().any(|l| l == "f  add admin table"),
        "a table need names the table fix: {:?}",
        table_card.detail
    );
    let neither_card = super::shunt_card(
        &slot,
        true,
        false,
        None,
        Some(&record),
        Some(crate::gateway::AdminNeed::Neither),
        None,
        None,
    );
    assert!(
        !neither_card
            .detail
            .iter()
            .any(|l| l.starts_with("f  add admin")),
        "no admin verb when the config already carries clauth's key"
    );

    // The pool note: one line per provider; a failure is one `pool` warn line.
    let two = super::PoolOutcome::Providers(vec!["codex".to_string(), "antigravity".to_string()]);
    let card = super::shunt_card(
        &slot,
        true,
        false,
        None,
        Some(&record),
        None,
        None,
        Some(&two),
    );
    assert!(
        card.detail
            .iter()
            .any(|l| l == "codex  provider codex needs its own pool login")
            && card
                .detail
                .iter()
                .any(|l| l == "codex  provider antigravity needs its own pool login"),
        "one note line per provider: {:?}",
        card.detail
    );
    let err = super::PoolOutcome::Error("no gateway admin token".to_string());
    let card = super::shunt_card(
        &slot,
        true,
        false,
        None,
        Some(&record),
        None,
        None,
        Some(&err),
    );
    assert!(
        card.detail
            .iter()
            .any(|l| l == "pool  no gateway admin token"),
        "a failed read is one `pool` line: {:?}",
        card.detail
    );
}

/// Every MOVE PLAN line shape: the `moves` source path, each `stays` reason,
/// the `NoHome` row, and the `refused` line — all in the rendered two-space
/// prose form, never a `key: value` split.
#[test]
fn the_move_plan_names_every_kept_reason_no_home_and_refusal() {
    use crate::gateway::{KeptFile, KeptReason, StoreMovePlan};
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);

    let plan = StoreMovePlan {
        moved: vec![
            moved_file("/standalone/a.json", "/clauth/a.json"),
            moved_file("/standalone/b.json", "/clauth/b.json"),
        ],
        kept: vec![
            KeptFile::At {
                path: "/standalone/c.json".into(),
                reason: KeptReason::LeftBehind,
            },
            KeptFile::At {
                path: "/standalone/d.json".into(),
                reason: KeptReason::CodexLogin,
            },
            KeptFile::At {
                path: "/standalone/e.json".into(),
                reason: KeptReason::ClauthOwned,
            },
            KeptFile::At {
                path: "/standalone/f.json".into(),
                reason: KeptReason::HardLink,
            },
            KeptFile::At {
                path: "/standalone/g.json".into(),
                reason: KeptReason::LinkCountUnreadable,
            },
            KeptFile::At {
                path: "/standalone/h.json".into(),
                reason: KeptReason::DuplicateSource,
            },
            KeptFile::NoHome {
                store: "SHUNT_XAI_AUTH_FILE",
            },
        ],
    };
    let card = super::shunt_card(
        &shunt_slot(GatewayState::Disabled),
        false,
        false,
        None,
        Some(&record),
        None,
        Some(&super::MovePlanOutcome::Plan(plan)),
        None,
    );
    let start = card.shunt_action_start.unwrap();
    assert_eq!(
        &card.detail[start..],
        vec![
            String::new(),
            "enabled".to_string(),
            String::new(),
            "MOVE PLAN".to_string(),
            "moves  /standalone/a.json".to_string(),
            "moves  /standalone/b.json".to_string(),
            "stays  /standalone/c.json  not an account file".to_string(),
            "stays  /standalone/d.json  the codex CLI's own login".to_string(),
            "stays  /standalone/e.json  already clauth's".to_string(),
            "stays  /standalone/f.json  linked under another name".to_string(),
            "stays  /standalone/g.json  link count unreadable".to_string(),
            "stays  /standalone/h.json  named twice".to_string(),
            "stays  SHUNT_XAI_AUTH_FILE  no home to find it: set HOME in the env file".to_string(),
            "f  move stores in".to_string(),
        ],
        "every MOVE PLAN line shape, verbatim"
    );
    assert!(
        card.shunt_focus.iter().any(|f| matches!(
            f,
            super::ShuntFocus::Fix {
                fix: super::ServiceFix::MoveStoresIn,
                ..
            }
        )),
        "the move fix walks"
    );

    // A refused plan shows the refusal, no move fix.
    let card = super::shunt_card(
        &shunt_slot(GatewayState::Disabled),
        false,
        false,
        None,
        Some(&record),
        None,
        Some(&super::MovePlanOutcome::Refused(
            "the stores changed since the plan was shown; look at the plan again".to_string(),
        )),
        None,
    );
    let start = card.shunt_action_start.unwrap();
    assert_eq!(
        &card.detail[start..],
        vec![
            String::new(),
            "enabled".to_string(),
            String::new(),
            "MOVE PLAN".to_string(),
            "refused  the stores changed since the plan was shown; look at the plan again"
                .to_string(),
        ],
        "the refusal line, no move offered"
    );
    assert!(
        !card.shunt_focus.iter().any(|f| matches!(
            f,
            super::ShuntFocus::Fix {
                fix: super::ServiceFix::MoveStoresIn,
                ..
            }
        )),
        "a refused plan offers no move"
    );

    // A kept-only plan (0 moves, no refusal) shows no MOVE PLAN at all.
    let card = super::shunt_card(
        &shunt_slot(GatewayState::Disabled),
        false,
        false,
        None,
        Some(&record),
        None,
        Some(&super::MovePlanOutcome::Nothing),
        None,
    );
    assert!(
        !card.detail.iter().any(|l| l == "MOVE PLAN"),
        "a kept-only plan shows no MOVE PLAN: {:?}",
        card.detail
    );
}

/// The held state reads what it means, not the feed's bare `held` word; the
/// other state words stay the `status.json` spelling.
#[test]
fn the_held_state_reads_stopped_until_the_daemon_restarts() {
    let _home = crate::testutil::HomeSandbox::new();
    let card = super::shunt_card(
        &shunt_slot(GatewayState::Held),
        true,
        false,
        None,
        None,
        None,
        None,
        None,
    );
    assert!(
        card.detail
            .iter()
            .any(|l| l == "state: stopped until the daemon restarts"),
        "the held card names the meaning: {:?}",
        card.detail
    );

    let mut foreign = shunt_slot(GatewayState::Foreign);
    foreign.answerer = Some(Answerer::Shunt);
    let card = super::shunt_card(&foreign, true, false, None, None, None, None, None);
    assert!(card.detail.iter().any(|l| l == "state: foreign"));
    assert!(card.detail.iter().any(|l| l == "answerer: shunt"));
    let card = super::shunt_card(
        &shunt_slot(GatewayState::BelowFloor),
        true,
        false,
        None,
        None,
        None,
        None,
        None,
    );
    assert!(card.detail.iter().any(|l| l == "state: below_floor"));
    let card = super::shunt_card(
        &shunt_slot(GatewayState::YamlRefused),
        true,
        false,
        None,
        None,
        None,
        None,
        None,
    );
    assert!(card.detail.iter().any(|l| l == "state: yaml_refused"));
}

// ── shunt card actions: the engine and the toast ────────────────────────────

/// A config with a bind the caller controls, for the adopt/move probe.
fn adopt_config(home: &crate::testutil::HomeSandbox, bind: u16) -> std::path::PathBuf {
    let config = home.home().join("etc").join("shunt.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, format!("[server]\nbind = \"127.0.0.1:{bind}\"\n")).unwrap();
    config
}

/// A closed loopback port: bind then drop, the `a_closed_port_reads_as_silent`
/// shape, so the action probe reads silent deterministically.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr.port()
}

/// Serve one `/health` answer (a shunt-shaped JSON body) on a fresh loopback
/// port, returning the addr and the thread that stops after one request. The
/// move's orphan-refusal probe makes exactly one `GET /health`.
fn serve_health_once(body: &'static str) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        if let Some(stream) = listener.incoming().next() {
            let mut stream = stream.unwrap();
            let mut buf = [0u8; 2048];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
        }
    });
    (addr, handle)
}

/// Adopting a config with nothing to move (no standalone stores) writes the
/// record enabled, so the daemon starts the gateway at once.
#[test]
fn adopting_with_nothing_to_move_leaves_the_gateway_enabled() {
    let home = crate::testutil::HomeSandbox::new();
    let config = adopt_config(&home, closed_port());
    let outcome = super::run_adopt(&config);
    assert!(
        matches!(outcome, super::ShuntActionOutcome::Adopted { .. }),
        "{outcome:?}"
    );
    let record = crate::gateway::GatewayRecord::load().unwrap().unwrap();
    assert!(!record.disabled, "nothing to move adopts enabled");
}

/// Adopting a config with a file to move writes the record disabled, so the
/// daemon never starts the gateway on empty stores until the move lands.
#[test]
fn adopting_with_a_file_to_move_leaves_the_gateway_disabled() {
    let home = crate::testutil::HomeSandbox::new();
    let store = home.home().join(".shunt").join("accounts").join("claude");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("main.json"), "{}").unwrap();
    let config = adopt_config(&home, closed_port());
    let outcome = super::run_adopt(&config);
    assert!(
        matches!(outcome, super::ShuntActionOutcome::Adopted { .. }),
        "{outcome:?}"
    );
    let record = crate::gateway::GatewayRecord::load().unwrap().unwrap();
    assert!(record.disabled, "a file to move adopts disabled");
}

/// A standalone shunt answering at the adopt probe refuses the adopt: nothing
/// is written, and the outcome carries the warning copy.
#[test]
fn adopting_into_a_standalone_shunt_that_answers_writes_nothing() {
    let home = crate::testutil::HomeSandbox::new();
    let (base, _handle) = crate::testutil::serve_endpoints(1, |path, _i| {
        assert_eq!(path, "/health");
        (200, r#"{"status":"ok","version":"0.49.1"}"#.to_string())
    });
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
    let config = adopt_config(&home, port);
    let outcome = super::run_adopt(&config);
    match outcome {
        super::ShuntActionOutcome::AdoptRefused { version, addr } => {
            assert_eq!(version, "0.49.1");
            assert_eq!(addr.port(), port);
        }
        other => panic!("expected an answering refusal, got {other:?}"),
    }
    assert!(
        crate::gateway::GatewayRecord::load().unwrap().is_none(),
        "nothing is written when a standalone answers"
    );
}

/// A store move over a silent probe moves the stores and leaves the record
/// enabled, so the daemon starts the gateway.
#[test]
fn a_successful_move_leaves_the_record_enabled() {
    let home = crate::testutil::HomeSandbox::new();
    let store = home.home().join(".shunt").join("accounts").join("claude");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("main.json"), "{}").unwrap();

    let mut record =
        crate::gateway::GatewayRecord::new(adopt_config(&home, closed_port())).unwrap();
    record.disabled = true;
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let confirmed = crate::gateway::plan_standalone_stores(&record).unwrap();
    assert_eq!(confirmed.moved.len(), 1, "fixture: one store file moves");

    let outcome = super::run_move(&record, &confirmed);
    match outcome {
        super::ShuntActionOutcome::Moved { n } => assert_eq!(n, 1),
        other => panic!("expected a move, got {other:?}"),
    }
    let record = crate::gateway::GatewayRecord::load().unwrap().unwrap();
    assert!(!record.disabled, "a successful move enables the gateway");
}

/// A move that refuses (the stores changed since the plan was shown) leaves
/// the record disabled.
#[test]
fn a_refused_move_leaves_the_record_disabled() {
    let home = crate::testutil::HomeSandbox::new();
    let store = home.home().join(".shunt").join("accounts").join("claude");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("main.json"), "{}").unwrap();

    let mut record =
        crate::gateway::GatewayRecord::new(adopt_config(&home, closed_port())).unwrap();
    record.disabled = true;
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let confirmed = crate::gateway::plan_standalone_stores(&record).unwrap();

    // The stores change after the plan is shown: the move re-plans and refuses.
    std::fs::write(store.join("extra.json"), "{}").unwrap();

    let outcome = super::run_move(&record, &confirmed);
    match outcome {
        super::ShuntActionOutcome::MoveFailed { error } => {
            assert_eq!(
                error, "the stores changed since the plan was shown; look at the plan again",
                "the PlanChanged refusal"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let record = crate::gateway::GatewayRecord::load().unwrap().unwrap();
    assert!(
        record.disabled,
        "a refused move leaves the gateway disabled"
    );
}

/// `space` on the focused `enabled` row flips `disabled` on disk at once, no
/// confirm.
#[test]
fn space_on_the_enabled_row_flips_disabled_on_disk() {
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();

    let mut app = bare_app();
    app.tab = super::Tab::Services;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    // Descend into the shunt detail and park on the enabled row (the first
    // actionable line).
    app.services.focus = super::ServicesFocus::Detail;
    app.services
        .shunt_focus
        .set(Some(super::ShuntFocusLine::Enabled));
    super::handle_services_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        !crate::gateway::GatewayRecord::load()
            .unwrap()
            .unwrap()
            .disabled,
        "space flips disabled off"
    );

    super::handle_services_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        crate::gateway::GatewayRecord::load()
            .unwrap()
            .unwrap()
            .disabled,
        "and back on"
    );
}

/// One shunt-card action's outcome maps to the approved toast: adopted,
/// answering, moved, admin added, and each failure.
#[test]
fn the_shunt_action_toasts_follow_the_outcome() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    let bodies = |app: &App| {
        app.toasts
            .iter()
            .map(|t| t.body.clone())
            .collect::<Vec<_>>()
    };

    super::handle_shunt_action_outcome(
        &mut app,
        super::ShuntActionOutcome::Adopted {
            path: "/cfg/shunt.toml".to_string(),
        },
    );
    assert!(
        bodies(&app).iter().any(|b| b == "adopted /cfg/shunt.toml"),
        "{:?}",
        bodies(&app)
    );
    app.toasts.clear();

    super::handle_shunt_action_outcome(
        &mut app,
        super::ShuntActionOutcome::AdoptRefused {
            version: "0.49.1".to_string(),
            addr: "127.0.0.1:3001".parse().unwrap(),
        },
    );
    assert!(
        bodies(&app)
            .iter()
            .any(|b| b == "shunt 0.49.1 answers on 127.0.0.1:3001\nstop it, then adopt again"),
        "{:?}",
        bodies(&app)
    );
    app.toasts.clear();

    super::handle_shunt_action_outcome(&mut app, super::ShuntActionOutcome::Moved { n: 3 });
    assert!(
        bodies(&app)
            .iter()
            .any(|b| b == "moved 3 account files into clauth"),
        "{:?}",
        bodies(&app)
    );
    app.toasts.clear();

    super::handle_shunt_action_outcome(&mut app, super::ShuntActionOutcome::AdminAdded);
    assert!(
        bodies(&app).iter().any(|b| b == "added clauth's admin key"),
        "{:?}",
        bodies(&app)
    );
    app.toasts.clear();

    super::handle_shunt_action_outcome(
        &mut app,
        super::ShuntActionOutcome::AdoptFailed {
            error: "boom".to_string(),
        },
    );
    assert!(
        bodies(&app).iter().any(|b| b == "adopt failed\nboom"),
        "{:?}",
        bodies(&app)
    );
    app.toasts.clear();

    super::handle_shunt_action_outcome(
        &mut app,
        super::ShuntActionOutcome::MoveFailed {
            error: "boom".to_string(),
        },
    );
    assert!(
        bodies(&app).iter().any(|b| b == "move failed\nboom"),
        "{:?}",
        bodies(&app)
    );
    app.toasts.clear();

    super::handle_shunt_action_outcome(
        &mut app,
        super::ShuntActionOutcome::AdminFailed {
            error: "boom".to_string(),
        },
    );
    assert!(
        bodies(&app).iter().any(|b| b == "admin key failed\nboom"),
        "{:?}",
        bodies(&app)
    );
}

/// The admin confirm hands the job to the action worker: the engine's
/// `add_admin_write_key`/`add_admin_table` reach the config behind the stub
/// `shunt check`, and the outcome lands as the admin-added toast.
#[cfg(unix)]
#[test]
fn the_admin_action_reaches_the_engine_and_toasts() {
    let home = crate::testutil::HomeSandbox::new();
    let stub = home.home().join("bin").join("shunt");
    std::fs::create_dir_all(stub.parent().unwrap()).unwrap();
    std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    let config = home.home().join("etc").join("shunt.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "[server]\n").unwrap();
    let mut record = crate::gateway::GatewayRecord::new(config.clone()).unwrap();
    record.binary = Some(stub);

    let mut app = bare_app();
    app.services.shunt_action.prober = Some(super::run_shunt_action);
    app.services
        .shunt_action
        .start(super::ShuntActionJob::AddAdminTable {
            record: record.clone(),
        });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.services.shunt_action.running {
        assert!(
            std::time::Instant::now() < deadline,
            "the admin action did not land"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_service_probes(&mut app);
    }
    super::drain_service_probes(&mut app);
    assert!(
        app.toasts
            .iter()
            .any(|t| t.body == "added clauth's admin key"),
        "the admin-added toast: {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(
        text.contains("id = \"clauth\""),
        "the engine wrote clauth's key:\n{text}"
    );
}

/// Each shunt confirm carries its approved two lines and hands the matching
/// `ConfirmAction` to the worker on confirm.
#[test]
fn the_shunt_confirms_carry_the_approved_copy() {
    use crate::gateway::{KeptFile, KeptReason, StoreMovePlan};
    use crate::tui::app::{ConfirmAction, Modal};
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);

    // adopt.
    let toml = super::StandaloneShunt {
        found: Some(std::path::PathBuf::from("/cfg/shunt.toml")),
        unread_bind: None,
        unread_config: None,
        unread_env: false,
        answer: None,
    };
    let card = super::shunt_card(
        &shunt_slot(GatewayState::Absent),
        false,
        false,
        Some(&toml),
        None,
        None,
        None,
        None,
    );
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.checks = vec![card];
    app.services.cursor = 0;
    super::apply_service_fix(&mut app);
    match app.modals.last() {
        Some(Modal::Confirm(state)) => {
            assert_eq!(state.message, "adopt /cfg/shunt.toml?");
            assert_eq!(
                state.detail.as_deref(),
                Some("clauth runs shunt on this config and edits it in place.")
            );
            assert!(matches!(state.on_confirm, ConfirmAction::AdoptConfig(_)));
        }
        other => panic!("expected the adopt confirm, got {other:?}"),
    }

    // move with K > 0: the second line names the count.
    let plan = StoreMovePlan {
        moved: vec![
            moved_file("/s/a.json", "/c/a.json"),
            moved_file("/s/b.json", "/c/b.json"),
        ],
        kept: vec![
            KeptFile::At {
                path: "/s/k.json".into(),
                reason: KeptReason::LeftBehind,
            },
            KeptFile::At {
                path: "/s/l.json".into(),
                reason: KeptReason::HardLink,
            },
        ],
    };
    let mut slot = shunt_slot(GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    let card = super::shunt_card(
        &slot,
        false,
        false,
        None,
        Some(&record),
        None,
        Some(&super::MovePlanOutcome::Plan(plan.clone())),
        None,
    );
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.checks = vec![card];
    app.services.cursor = 0;
    app.services.move_plan = Some(super::MovePlanOutcome::Plan(plan));
    super::apply_service_fix(&mut app);
    match app.modals.last() {
        Some(Modal::Confirm(state)) => {
            assert_eq!(state.message, "move 2 account files into clauth?");
            assert_eq!(
                state.detail.as_deref(),
                Some("2 stay behind, listed on the card.")
            );
            assert!(matches!(state.on_confirm, ConfirmAction::MoveStoresIn(_)));
        }
        other => panic!("expected the move confirm, got {other:?}"),
    }

    // move with K = 0: the second line is absent.
    let plan = StoreMovePlan {
        moved: vec![moved_file("/s/a.json", "/c/a.json")],
        kept: vec![],
    };
    let card = super::shunt_card(
        &slot,
        false,
        false,
        None,
        Some(&record),
        None,
        Some(&super::MovePlanOutcome::Plan(plan.clone())),
        None,
    );
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.checks = vec![card];
    app.services.cursor = 0;
    app.services.move_plan = Some(super::MovePlanOutcome::Plan(plan));
    super::apply_service_fix(&mut app);
    match app.modals.last() {
        Some(Modal::Confirm(state)) => {
            assert_eq!(state.message, "move 1 account file into clauth?");
            assert_eq!(state.detail, None, "K = 0 drops the stay-behind line");
            assert!(matches!(state.on_confirm, ConfirmAction::MoveStoresIn(_)));
        }
        other => panic!("expected the move confirm, got {other:?}"),
    }

    // admin key.
    let card = super::shunt_card(
        &slot,
        false,
        false,
        None,
        Some(&record),
        Some(crate::gateway::AdminNeed::WriteKey),
        None,
        None,
    );
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.checks = vec![card];
    app.services.cursor = 0;
    super::apply_service_fix(&mut app);
    match app.modals.last() {
        Some(Modal::Confirm(state)) => {
            assert_eq!(state.message, "add clauth's admin key to this config?");
            assert_eq!(
                state.detail.as_deref(),
                Some("clauth needs it to manage pool accounts and read their usage.")
            );
            assert!(matches!(state.on_confirm, ConfirmAction::AddAdminKey));
        }
        other => panic!("expected the admin-key confirm, got {other:?}"),
    }

    // admin table.
    let card = super::shunt_card(
        &slot,
        false,
        false,
        None,
        Some(&record),
        Some(crate::gateway::AdminNeed::AdminTable),
        None,
        None,
    );
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.checks = vec![card];
    app.services.cursor = 0;
    super::apply_service_fix(&mut app);
    match app.modals.last() {
        Some(Modal::Confirm(state)) => {
            assert_eq!(
                state.message,
                "add a [server.admin] table with clauth's key?"
            );
            assert_eq!(
                state.detail.as_deref(),
                Some("the gateway restarts once to load it.")
            );
            assert!(matches!(state.on_confirm, ConfirmAction::AddAdminTable));
        }
        other => panic!("expected the admin-table confirm, got {other:?}"),
    }
}

// ── round 2: off matrix, stale caches, coverage plants P1-P7 ────────────────

/// P1: a MOVE PLAN shows only while the managed gateway is off — a supervised
/// healthy enabled gateway with a plan shows none.
#[test]
fn a_running_gateway_shows_no_move_plan() {
    use crate::gateway::StoreMovePlan;
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, false);
    let mut slot = shunt_slot(GatewayState::Healthy);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let plan = StoreMovePlan {
        moved: vec![moved_file("/s/a.json", "/c/a.json")],
        kept: vec![],
    };
    let card = super::shunt_card(
        &slot,
        true,
        false,
        None,
        Some(&record),
        None,
        Some(&super::MovePlanOutcome::Plan(plan)),
        None,
    );
    assert!(
        !card.detail.iter().any(|l| l == "MOVE PLAN"),
        "a running gateway hides MOVE PLAN: {:?}",
        card.detail
    );

    // The same plan shows once the slot is a no-child state (disabled).
    let disabled = shunt_slot(GatewayState::Disabled);
    let plan = StoreMovePlan {
        moved: vec![moved_file("/s/a.json", "/c/a.json")],
        kept: vec![],
    };
    let card = super::shunt_card(
        &disabled,
        true,
        false,
        None,
        Some(&record),
        None,
        Some(&super::MovePlanOutcome::Plan(plan)),
        None,
    );
    assert!(
        card.detail.iter().any(|l| l == "MOVE PLAN"),
        "a disabled gateway shows MOVE PLAN: {:?}",
        card.detail
    );
}

/// P2: the move confirm counts N and K separately (the stay-behind line is
/// K, not N).
#[test]
fn the_move_confirm_counts_n_and_k_separately() {
    use crate::gateway::{KeptFile, KeptReason, StoreMovePlan};
    use crate::tui::app::Modal;
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);
    let plan = StoreMovePlan {
        moved: vec![
            moved_file("/s/a.json", "/c/a.json"),
            moved_file("/s/b.json", "/c/b.json"),
        ],
        kept: vec![KeptFile::At {
            path: "/s/k.json".into(),
            reason: KeptReason::LeftBehind,
        }],
    };
    let mut slot = shunt_slot(GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    let card = super::shunt_card(
        &slot,
        false,
        false,
        None,
        Some(&record),
        None,
        Some(&super::MovePlanOutcome::Plan(plan.clone())),
        None,
    );
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.checks = vec![card];
    app.services.cursor = 0;
    app.services.move_plan = Some(super::MovePlanOutcome::Plan(plan));
    super::apply_service_fix(&mut app);
    match app.modals.last() {
        Some(Modal::Confirm(state)) => {
            assert_eq!(state.message, "move 2 account files into clauth?");
            assert_eq!(
                state.detail.as_deref(),
                Some("1 stays behind, listed on the card.")
            );
        }
        other => panic!("expected the move confirm, got {other:?}"),
    }
}

/// P3: a landed move drops the stale plan cache, so the next confirm counts a
/// fresh plan.
#[test]
fn a_landed_move_drops_the_stale_plan() {
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.daemon_health = crate::daemon::DaemonHealth::Absent;
    app.services.shunt_record = Some(record.clone());
    let stale = crate::gateway::StoreMovePlan {
        moved: vec![moved_file("/s/a.json", "/c/a.json")],
        kept: vec![],
    };
    app.services.move_plan = Some(super::MovePlanOutcome::Plan(stale));
    fn stub(_job: super::ShuntActionJob) -> super::ShuntActionOutcome {
        super::ShuntActionOutcome::Moved { n: 1 }
    }
    app.services.shunt_action.prober = Some(stub);
    app.services
        .shunt_action
        .start(super::ShuntActionJob::AddAdminKey { record });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.services.shunt_action.running {
        assert!(
            std::time::Instant::now() < deadline,
            "the move action did not land"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_service_probes(&mut app);
    }
    super::drain_service_probes(&mut app);
    assert!(
        app.services.move_plan.is_none(),
        "a landed move drops the stale plan: {:?}",
        app.services.move_plan
    );
}

/// P4: the pool read runs only when no admin step is owed (the config already
/// carries clauth's admin key).
#[test]
fn the_pool_read_requires_an_admin_step_of_neither() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static POOL_RUNS: AtomicUsize = AtomicUsize::new(0);
    fn pool_stub() -> super::PoolOutcome {
        POOL_RUNS.fetch_add(1, Ordering::SeqCst);
        super::PoolOutcome::Providers(Vec::new())
    }
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, false);
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let status_path = dir.join("status.json");
    let feed = serde_json::json!({
        "gateway": { "state": "healthy", "floor": "0.48.0", "restarts": 0 }
    });
    std::fs::write(&status_path, serde_json::to_vec(&feed).expect("serialize")).expect("write");

    POOL_RUNS.store(0, Ordering::SeqCst);
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    app.services.pool_probe.prober = Some(pool_stub);
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert_eq!(
        app.services.admin_need,
        Some(crate::gateway::AdminNeed::AdminTable),
        "fixture: the config has no [server.admin] table"
    );
    // The worker-start decision is the deterministic signal, not a spawned
    // thread's counter: an owed admin step must never start the pool worker.
    assert!(
        !app.services.pool_probe.running,
        "no pool read while an admin step is owed"
    );
    super::drain_service_probes(&mut app);
    assert_eq!(
        POOL_RUNS.load(Ordering::SeqCst),
        0,
        "the pool stub never ran while an admin step is owed"
    );
    assert!(
        app.services.pool.is_none(),
        "no cached pool read while an admin step is owed"
    );
}

/// P5/P7: the key action runs the key edit (the engine's `add_admin_write_key`),
/// never the table edit — on a config needing the key, the table edit would
/// refuse `Needs(WriteKey)`.
#[cfg(unix)]
#[test]
fn the_key_action_runs_the_key_edit() {
    let home = crate::testutil::HomeSandbox::new();
    let stub = home.home().join("bin").join("shunt");
    std::fs::create_dir_all(stub.parent().unwrap()).unwrap();
    std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    let config = home.home().join("etc").join("shunt.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "[server.admin]\n").unwrap();
    let mut record = crate::gateway::GatewayRecord::new(config.clone()).unwrap();
    record.binary = Some(stub);

    let outcome = super::run_shunt_action(super::ShuntActionJob::AddAdminKey { record });
    match outcome {
        super::ShuntActionOutcome::AdminAdded => {}
        other => panic!("the key action adds the key, got {other:?}"),
    }
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(
        text.contains("id = \"clauth\""),
        "the engine wrote clauth's key:\n{text}"
    );
}

/// P5: the `add admin key` confirm dispatches the key JOB, never the table
/// job (the table edit would refuse `Needs(WriteKey)` on a key-needed config).
#[test]
fn the_key_confirm_dispatches_the_key_job() {
    use crate::tui::app::{ConfirmAction, ShuntActionJob, ShuntActionOutcome};
    use std::sync::Mutex;
    static CAPTURED: Mutex<Option<ShuntActionJob>> = Mutex::new(None);
    fn capture(job: ShuntActionJob) -> ShuntActionOutcome {
        *CAPTURED.lock().unwrap() = Some(job);
        ShuntActionOutcome::AdminAdded
    }
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);
    *CAPTURED.lock().unwrap() = None;
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.shunt_record = Some(record.clone());
    app.services.shunt_action.prober = Some(capture);
    super::run_confirm_action(&mut app, ConfirmAction::AddAdminKey);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.services.shunt_action.running {
        assert!(
            std::time::Instant::now() < deadline,
            "the key action did not land"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_service_probes(&mut app);
    }
    super::drain_service_probes(&mut app);
    let job = CAPTURED
        .lock()
        .unwrap()
        .clone()
        .expect("the key confirm dispatched a job");
    assert!(
        matches!(job, ShuntActionJob::AddAdminKey { .. }),
        "the key confirm dispatches the key job, got {job:?}"
    );
}

/// The `enabled` toggle's failed record update keeps the cause chain: with a
/// directory where the record belongs, the update's read fails and the toast
/// names the path and, under it, the OS's own reason.
#[test]
fn a_failed_enabled_toggle_keeps_the_cause_under_its_context() {
    let home = crate::testutil::HomeSandbox::new();
    let record = home.home().join(".clauth").join("gateway.toml");
    std::fs::create_dir_all(&record).unwrap();
    let cause = std::fs::read_to_string(&record).expect_err("a directory");
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    super::toggle_shunt_enabled(&mut app);
    assert_eq!(
        app.toasts
            .iter()
            .map(|t| (t.kind, t.body.clone()))
            .collect::<Vec<_>>(),
        vec![(
            super::ToastKind::Danger,
            format!("save failed\nfailed to read {}: {cause}", record.display())
        )]
    );
}

/// A card confirm whose adopted record vanished before the yes (another
/// surface un-adopted the config) toasts why it did nothing, under its own
/// failure head, and dispatches no job.
#[test]
fn a_card_confirm_on_a_vanished_record_names_the_missing_config() {
    use crate::tui::app::{ConfirmAction, ShuntActionJob, ShuntActionOutcome};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static JOBS: AtomicUsize = AtomicUsize::new(0);
    fn count(_: ShuntActionJob) -> ShuntActionOutcome {
        JOBS.fetch_add(1, Ordering::SeqCst);
        ShuntActionOutcome::NoOp
    }
    let _home = crate::testutil::HomeSandbox::new();
    JOBS.store(0, Ordering::SeqCst);
    let plan = crate::gateway::StoreMovePlan {
        moved: Vec::new(),
        kept: Vec::new(),
    };
    for (action, toast) in [
        (
            ConfirmAction::MoveStoresIn(plan),
            "move failed\nno shunt config is adopted any more",
        ),
        (
            ConfirmAction::AddAdminKey,
            "admin key failed\nno shunt config is adopted any more",
        ),
        (
            ConfirmAction::AddAdminTable,
            "admin key failed\nno shunt config is adopted any more",
        ),
    ] {
        let mut app = bare_app();
        app.tab = super::Tab::Services;
        app.services.shunt_record = None;
        app.services.shunt_action.prober = Some(count);
        super::run_confirm_action(&mut app, action);
        assert_eq!(
            app.toasts
                .iter()
                .map(|t| (t.kind, t.body.as_str()))
                .collect::<Vec<_>>(),
            vec![(super::ToastKind::Danger, toast)]
        );
        assert!(!app.services.shunt_action.running, "{toast}: no job starts");
    }
    assert_eq!(JOBS.load(Ordering::SeqCst), 0);
}

/// Every string from outside clauth's own code on the card escapes control and
/// bidi characters: a provider name carrying U+202E and a control byte renders
/// as its visible `\u{…}` form.
#[test]
fn the_shunt_card_escapes_control_and_bidi_in_a_provider_name() {
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, false);
    let mut slot = shunt_slot(GatewayState::Healthy);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let provider = "a\u{202e}b\u{07}c";
    let card = super::shunt_card(
        &slot,
        true,
        false,
        None,
        Some(&record),
        None,
        None,
        Some(&super::PoolOutcome::Providers(vec![provider.to_string()])),
    );
    assert!(
        card.detail
            .iter()
            .any(|l| l == "codex  provider a\\u{202e}b\\u{7}c needs its own pool login"),
        "the provider name renders escaped: {:?}",
        card.detail
    );
}

// ── round 3: card keys, off, the daemon-health restart ──────────────────────

/// Q3 + ruling 2: a dead daemon reads the card OFF even over a slot that still
/// looks healthy (a stale feed's gateway may answer), so MOVE PLAN shows and
/// the move's own probe — not the feed's freshness — refuses the orphan.
#[test]
fn a_dead_daemon_reads_the_card_off_over_a_stale_healthy_slot() {
    use crate::gateway::StoreMovePlan;
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, false);
    let mut slot = shunt_slot(GatewayState::Healthy);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let plan = StoreMovePlan {
        moved: vec![moved_file("/s/a.json", "/c/a.json")],
        kept: vec![],
    };
    let card = super::shunt_card(
        &slot,
        true,
        /*daemon_absent=*/ true,
        None,
        Some(&record),
        None,
        Some(&super::MovePlanOutcome::Plan(plan)),
        None,
    );
    assert!(
        card.detail.iter().any(|l| l == "MOVE PLAN"),
        "a dead daemon reads off (MOVE PLAN) over a stale healthy slot: {:?}",
        card.detail
    );
}

/// Ruling 2: the move's own `/health` probe refuses an orphaned gateway still
/// answering after its daemon died — `move failed` + the approved
/// `shunt <version> answers on <bind>` toast, the card having shown MOVE PLAN.
#[test]
fn the_move_refuses_an_orphaned_gateway_still_answering() {
    let home = crate::testutil::HomeSandbox::new();
    let (addr, serve) = serve_health_once(r#"{"version":"0.49.1"}"#);
    let config = adopt_config(&home, addr.port());
    let record = crate::gateway::GatewayRecord::new(config).unwrap();
    let confirmed = crate::gateway::StoreMovePlan {
        moved: vec![],
        kept: vec![],
    };
    let outcome = super::run_move(&record, &confirmed);
    match outcome {
        super::ShuntActionOutcome::MoveFailed { error } => {
            assert_eq!(
                error,
                format!("shunt 0.49.1 answers on {addr}"),
                "the orphan refusal"
            );
        }
        other => panic!("expected the orphan refusal, got {other:?}"),
    }
    serve.join().unwrap();

    let mut app = bare_app();
    app.tab = super::Tab::Services;
    super::handle_shunt_action_outcome(
        &mut app,
        super::ShuntActionOutcome::MoveFailed {
            error: format!("shunt 0.49.1 answers on {addr}"),
        },
    );
    assert!(
        app.toasts.iter().any(|t| {
            t.kind == super::ToastKind::Danger
                && t.body == format!("move failed\nshunt 0.49.1 answers on {addr}")
        }),
        "the move-failed toast: {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );
}

/// Q1: a FAILED action (not just a success) drops the stale plan cache and
/// re-runs the plan worker, so the next confirm counts a fresh plan.
#[test]
fn a_failed_action_replans() {
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.daemon_health = crate::daemon::DaemonHealth::Absent;
    app.services.shunt_record = Some(record.clone());
    let stale = crate::gateway::StoreMovePlan {
        moved: vec![moved_file("/s/a.json", "/c/a.json")],
        kept: vec![],
    };
    app.services.move_plan = Some(super::MovePlanOutcome::Plan(stale));
    fn stub(_job: super::ShuntActionJob) -> super::ShuntActionOutcome {
        super::ShuntActionOutcome::MoveFailed {
            error: "boom".to_string(),
        }
    }
    app.services.shunt_action.prober = Some(stub);
    app.services
        .shunt_action
        .start(super::ShuntActionJob::AddAdminKey { record });
    await_service_probes(&mut app);
    assert!(
        app.services.move_plan.is_none(),
        "a failed action drops the stale plan: {:?}",
        app.services.move_plan
    );
}

/// Q2: entering the tab re-runs the plan and pool workers (drops their caches)
/// so a card left stale by an action elsewhere reads fresh on re-entry.
#[test]
fn entering_the_tab_replans() {
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let mut app = bare_app();
    app.tab = super::Tab::Overview;
    app.daemon_health = crate::daemon::DaemonHealth::Absent;
    let stale = crate::gateway::StoreMovePlan {
        moved: vec![moved_file("/s/a.json", "/c/a.json")],
        kept: vec![],
    };
    app.services.move_plan = Some(super::MovePlanOutcome::Plan(stale));
    super::switch_tab(&mut app, super::Tab::Services);
    assert!(
        app.services.move_plan.is_none(),
        "tab entry drops the stale plan: {:?}",
        app.services.move_plan
    );
}

/// R2-5: a daemon start or stop (the chip's health changing) restarts the plan
/// worker, so `the running daemon recorded no environment; restart it, then
/// look at the plan again` clears without `r`.
#[test]
fn a_daemon_health_change_replans() {
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.daemon_health = crate::daemon::DaemonHealth::Absent;
    let stale = crate::gateway::StoreMovePlan {
        moved: vec![moved_file("/s/a.json", "/c/a.json")],
        kept: vec![],
    };
    app.services.move_plan = Some(super::MovePlanOutcome::Plan(stale));
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        app.services.move_plan.is_some(),
        "fixture: Keep keeps the stale plan"
    );
    // The daemon starts: the health flips, and the plan's env source with it.
    app.daemon_health = crate::daemon::DaemonHealth::Fresh;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    assert!(
        app.services.move_plan.is_none(),
        "a daemon health change drops the stale plan: {:?}",
        app.services.move_plan
    );
}

/// `↵` mirrors `space` on the focused `enabled` row, toggling `disabled` on
/// disk at once (the herdr options' `space/↵` precedent).
#[test]
fn enter_toggles_the_enabled_row() {
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();

    let mut app = bare_app();
    app.tab = super::Tab::Services;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    app.services.focus = super::ServicesFocus::Detail;
    app.services
        .shunt_focus
        .set(Some(super::ShuntFocusLine::Enabled));
    super::handle_services_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        !crate::gateway::GatewayRecord::load()
            .unwrap()
            .unwrap()
            .disabled,
        "enter flips disabled off"
    );
}

/// A failed `enabled` write toasts the house `save failed` + the error. The
/// failure is the state flock (`~/.clauth/.lock` is a directory, which no
/// uid — root included — can open read+write), so the record still loads
/// (the card keeps its `enabled` row) while the toggle's write path fails,
/// and the test holds under root too. No 0500 to restore on panic.
#[cfg(unix)]
#[test]
fn a_failed_toggle_toasts_save_failed() {
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let config = adopt_config(&home, closed_port());
    let record = crate::gateway::GatewayRecord::new(config).unwrap();
    let dir = crate::profile::clauth_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        crate::gateway::record_path().unwrap(),
        toml::to_string_pretty(&record).unwrap(),
    )
    .unwrap();
    // A directory at the state-lock path: the record loads, the toggle's
    // `GatewayRecord::update` cannot open the lock file.
    std::fs::create_dir(dir.join(".lock")).unwrap();

    let mut app = bare_app();
    app.tab = super::Tab::Services;
    super::recompute_services_checks(&mut app, false, super::ShuntRefresh::Keep);
    app.services.focus = super::ServicesFocus::Detail;
    app.services
        .shunt_focus
        .set(Some(super::ShuntFocusLine::Enabled));
    super::handle_services_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == super::ToastKind::Danger && t.body.starts_with("save failed\n")),
        "a failed toggle toasts `save failed`: {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );
}

/// The `add admin key` action's landing, without the real `shunt check`: writes
/// the write-keys entry clauth's recompute reads back as `Neither`, so the
/// card rebuild drops the admin fix line exactly as a successful action would.
fn admin_landing_prober(job: super::ShuntActionJob) -> super::ShuntActionOutcome {
    let super::ShuntActionJob::AddAdminKey { record } = job else {
        panic!("the admin landing expects the add-admin-key job");
    };
    let token = crate::gateway::admin_token_path().unwrap();
    let key_ref = format!("${{file:{}}}", token.to_str().unwrap());
    std::fs::write(
        record.config(),
        format!(
            "[server]\n\n[server.admin]\nwrite_keys = [{{ id = \"clauth\", key = \"{key_ref}\" }}]\n"
        ),
    )
    .unwrap();
    super::ShuntActionOutcome::AdminAdded
}

/// The `move stores in` action's landing: the drain's `Restart` drops the
/// move-plan cache, which removes the MOVE PLAN section from the card — the
/// same recompute path a real landed move drives.
fn move_landing_prober(job: super::ShuntActionJob) -> super::ShuntActionOutcome {
    let super::ShuntActionJob::Move { confirmed, .. } = job else {
        panic!("the move landing expects the move job");
    };
    super::ShuntActionOutcome::Moved {
        n: confirmed.moved.len(),
    }
}

/// The rendered footer row of a full frame at `w`×`h`. The single draw both
/// re-settles the shunt focus (in the body pass) and renders the footer off
/// the re-settled identity, so this is the frame the user sees.
fn footer_row(app: &App, w: u16, h: u16) -> String {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, app)).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
        .last()
        .cloned()
        .unwrap_or_default()
}

/// The common path the review derived: a successful `add admin key` removes its
/// own line, so the drain's recompute rebuilds the card with only `enabled`
/// and the none focus re-settles to `enabled` — the footer shows
/// `space/↵ toggle`, no `f` verb.
#[test]
fn a_landed_add_admin_key_re_settles_focus_to_the_enabled_row() {
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);
    crate::gateway::GatewayRecord::update(|s| {
        *s = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let mut slot = shunt_slot(GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());

    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.checks = vec![super::shunt_card(
        &slot,
        false,
        false,
        None,
        Some(&record),
        Some(crate::gateway::AdminNeed::WriteKey),
        None,
        None,
    )];
    app.services.shunt_record = Some(record.clone());
    app.services.shunt_action.prober = Some(admin_landing_prober);

    // Descend onto `enabled`, walk to the admin fix, then confirm it.
    let _ = footer_row(&app, 120, 24);
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(
        app.services
            .focused_fix()
            .is_some_and(|f| matches!(f, super::ServiceFix::AddAdminKey)),
        "the walk lands on the admin fix"
    );
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Char('y')));
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Enter));

    super::join_test_workers();
    super::drain_service_probes(&mut app);

    // The drain's recompute rebuilt the card without the admin line (focus
    // none); the next frame re-settles the none focus to `enabled` and the
    // footer offers the re-settled line's verb.
    assert_eq!(app.services.shunt_focus.get(), None);
    let footer = footer_row(&app, 120, 24);
    assert_eq!(
        app.services.shunt_focus.get(),
        Some(super::ShuntFocusLine::Enabled)
    );
    assert!(
        footer.contains("space/↵ toggle"),
        "the footer offers the re-settled enabled row's verb: {footer}"
    );
}

/// The common path's move twin: a landed `move stores in` removes the MOVE PLAN
/// (its own line), so the drain's recompute rebuilds the card with only
/// `enabled` and the none focus re-settles to `enabled`.
#[test]
fn a_landed_move_re_settles_focus_to_the_enabled_row() {
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let record = adopted_record(&home, true);
    crate::gateway::GatewayRecord::update(|s| {
        *s = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let mut slot = shunt_slot(GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let plan = crate::gateway::StoreMovePlan {
        moved: vec![moved_file(
            "/home/u/.shunt/accounts/main.json",
            "/home/u/.clauth/shunt/main.json",
        )],
        kept: vec![],
    };

    let mut app = bare_app();
    app.tab = super::Tab::Services;
    app.services.checks = vec![super::shunt_card(
        &slot,
        false,
        false,
        None,
        Some(&record),
        None,
        Some(&super::MovePlanOutcome::Plan(plan.clone())),
        None,
    )];
    app.services.shunt_record = Some(record.clone());
    app.services.move_plan = Some(super::MovePlanOutcome::Plan(plan));
    app.services.shunt_action.prober = Some(move_landing_prober);

    // Descend onto `enabled`, walk to the move fix, then confirm it.
    let _ = footer_row(&app, 120, 24);
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(
        app.services
            .focused_fix()
            .is_some_and(|f| matches!(f, super::ServiceFix::MoveStoresIn)),
        "the walk lands on the move fix"
    );
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Char('y')));
    super::handle_key(&mut app, crate::testutil::key(KeyCode::Enter));

    super::join_test_workers();
    super::drain_service_probes(&mut app);

    // The drain's recompute rebuilt the card without the MOVE PLAN (focus
    // none); the next frame re-settles the none focus to `enabled` and the
    // footer offers the re-settled line's verb.
    assert_eq!(app.services.shunt_focus.get(), None);
    let footer = footer_row(&app, 120, 24);
    assert_eq!(
        app.services.shunt_focus.get(),
        Some(super::ShuntFocusLine::Enabled)
    );
    assert!(
        footer.contains("space/↵ toggle"),
        "the footer offers the re-settled enabled row's verb: {footer}"
    );
}

/// `MovedEnableFailed`: the stores moved, so `moved <N> …` still shows, then
/// the house `save failed` toast follows (the owner's F14 composition).
#[test]
fn a_moved_then_failed_enable_toasts_both() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    super::handle_shunt_action_outcome(
        &mut app,
        super::ShuntActionOutcome::MovedEnableFailed {
            n: 1,
            error: "boom".to_string(),
        },
    );
    let bodies: Vec<_> = app
        .toasts
        .iter()
        .map(|t| (t.kind, t.body.clone()))
        .collect();
    assert_eq!(
        bodies,
        vec![
            (
                super::ToastKind::Success,
                "moved 1 account file into clauth".to_string()
            ),
            (super::ToastKind::Danger, "save failed\nboom".to_string()),
        ],
        "the move-then-failed-enable toast pair"
    );
}

/// Q9 end-to-end: `run_move` whose move lands but whose record write then
/// fails reads `MovedEnableFailed` (not `MoveFailed`). The fixture is the
/// review's, root-proof: a pre-created, writable `~/.clauth/shunt/accounts/
/// claude/` takes the moved file, then a directory at the record's own path
/// (`gateway.toml`) fails the `disabled = false` record write for any uid —
/// the move lands, the enable write fails.
#[cfg(unix)]
#[test]
fn a_landed_move_whose_record_write_fails_reads_moved_then_save_failed() {
    let home = crate::testutil::HomeSandbox::new();
    // The standalone store the plan will move.
    let src = home.home().join(".shunt").join("accounts").join("claude");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.json"), "claude-main").unwrap();
    // A record on a silent bind, written to disk, `disabled` so the move is due.
    let config = adopt_config(&home, closed_port());
    let mut record = crate::gateway::GatewayRecord::new(config).unwrap();
    record.disabled = true;
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();

    let plan = crate::gateway::plan_standalone_stores(&record).expect("plan");
    assert!(!plan.moved.is_empty(), "fixture: the plan moves a file");

    // Pre-create the destination subtree (writable) so the move lands, then
    // make the record's own write fail root-proof: a directory at the record
    // path (`gateway.toml`) fails `GatewayRecord::update`'s `load` with EISDIR
    // for any uid (root included), while the moved file (under
    // `~/.clauth/shunt/`) still lands. No 0500 to restore on panic.
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(clauth.join("shunt").join("accounts").join("claude")).unwrap();
    let record_file = crate::gateway::record_path().unwrap();
    std::fs::remove_file(&record_file).unwrap();
    std::fs::create_dir(&record_file).unwrap();

    let outcome = super::run_move(&record, &plan);
    match outcome {
        super::ShuntActionOutcome::MovedEnableFailed { n, .. } => {
            assert_eq!(n, plan.moved.len(), "the move landed");
        }
        other => panic!("expected MovedEnableFailed, got {other:?}"),
    }
    // The moved file really landed before the record write failed.
    assert_eq!(
        std::fs::read_to_string(
            clauth
                .join("shunt")
                .join("accounts")
                .join("claude")
                .join("main.json")
        )
        .unwrap(),
        "claude-main",
        "the credential moved before the enable write"
    );
}

/// Q11: a `MoveFailed` error carrying control/bidi chars renders escaped in
/// the toast (an untrusted `Display`), like the card's provider names.
#[test]
fn the_move_failed_toast_escapes_control_and_bidi() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = super::Tab::Services;
    super::handle_shunt_action_outcome(
        &mut app,
        super::ShuntActionOutcome::MoveFailed {
            error: "a\u{202e}b\u{07}c".to_string(),
        },
    );
    assert!(
        app.toasts.iter().any(|t| {
            t.kind == super::ToastKind::Danger && t.body == "move failed\na\\u{202e}b\\u{7}c"
        }),
        "the move-failed toast escapes control/bidi: {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>()
    );
}

// ── keys and commands inside prose (cloudy-tui) ──────────────────────────────

/// The whole frame at `w`×`h`, for the prose-styling pins below.
fn prose_frame(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, app)).unwrap();
    term.backend().buffer().clone()
}

/// Every app-built line naming a key or a command, drawn through its real
/// surface: the divergence banner's `d`, the armed quit's `q`, the hidden-rows
/// toast's `c`, and `herdr config check` in both herdr confirms.
#[test]
fn app_prose_styles_keys_and_commands_on_every_surface() {
    use crate::testutil::assert_prose_part;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);

    let mut app = bare_app();
    let notice = super::DivergenceNotice {
        active: "work".to_string(),
        sibling: Some(crate::profile::ProfileName::from("play")),
        fingerprint: None,
    };
    app.banner = Some(super::Banner {
        severity: super::BannerSeverity::Warning,
        message: notice.banner_message(),
    });
    assert_prose_part(
        &prose_frame(&app, 120, 30),
        "press d to resolve",
        " d ",
        true,
    );
    app.banner = None;

    super::handle_key(&mut app, crate::testutil::key(super::KeyCode::Char('q')));
    assert!(app.armed_quit, "the first q arms the quit");
    assert_prose_part(
        &prose_frame(&app, 120, 30),
        "press q again to quit",
        " q ",
        true,
    );
    app.disarm_quit();

    let heal = super::herdr_check(
        &healthy_herdr_probe(),
        Some(&herdr_config(true, None, SidebarState::Templated)),
    );
    app.tab = Tab::Services;
    app.services.checks = vec![heal];
    app.services.cursor = 0;
    super::apply_service_fix(&mut app);
    assert!(matches!(app.modals.last(), Some(super::Modal::Confirm(_))));
    assert_prose_part(
        &prose_frame(&app, 200, 30),
        "validates the result with herdr config check.",
        "herdr config check",
        false,
    );
}

/// The help modal's description column names keys ACCENT + bold inside its
/// TEXT prose; the key column keeps its own slot.
#[test]
fn help_descriptions_style_the_keys_they_name() {
    use crate::testutil::assert_prose_part;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = bare_app();
    app.tab = Tab::Setup;
    app.open_modal(super::Modal::Help);
    let buf = prose_frame(&app, 140, 80);
    assert_prose_part(&buf, "edit inline; ↵ again saves", "↵", true);
    assert_prose_part(&buf, "+ add env · ↵ edits a value", "↵", true);
    assert_prose_part(&buf, "↵ once to arm, again to confirm", "↵", true);
    assert_prose_part(&buf, "(shift tab: previous)", "shift tab", true);

    app.modals.pop();
    app.tab = Tab::Config;
    app.open_modal(super::Modal::Help);
    assert_prose_part(
        &prose_frame(&app, 140, 80),
        "same as space ·",
        "space",
        true,
    );
}

/// The sites the first prose pin does not reach: the update toast's command,
/// the banner's no-owner arm, and both arms of the herdr row-text confirm.
#[test]
fn update_banner_and_row_text_prose_style_their_keys_and_commands() {
    use super::{KeyCode, Modal, handle_key};
    use crate::testutil::assert_prose_part;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);

    let mut app = bare_app();
    let (tx, rx) = std::sync::mpsc::channel();
    app.update_results = rx;
    tx.send(crate::update::UpdateEvent::Available("9.9.9".to_string()))
        .expect("send");
    super::on_tick(&mut app);
    assert_prose_part(
        &prose_frame(&app, 120, 30),
        "reinstall with cargo install clauth",
        "cargo install clauth",
        false,
    );
    app.toasts.clear();

    let notice = super::DivergenceNotice {
        active: "work".to_string(),
        sibling: None,
        fingerprint: None,
    };
    app.banner = Some(super::Banner {
        severity: super::BannerSeverity::Warning,
        message: notice.banner_message(),
    });
    assert_prose_part(
        &prose_frame(&app, 120, 30),
        "no longer matches 'work' · press d to resolve",
        " d ",
        true,
    );

    let tmp = tempfile::tempdir_in(home.home()).expect("tempdir");
    let config_path = tmp.path().join("herdr").join("config.toml");
    std::fs::create_dir_all(config_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&config_path, "# my config\n").expect("write base config");
    let mut app = herdr_options_app_with_config(&config_path);
    for turning_on in [true, false] {
        app.config().state.herdr.delegate_row_text = !turning_on;
        handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
        let Some(Modal::Confirm(state)) = app.modals.last() else {
            panic!("space opens the row-text confirm");
        };
        assert_eq!(
            state.message.starts_with("add "),
            turning_on,
            "the {turning_on} arm opened: {:?}",
            state.message
        );
        assert_prose_part(
            &prose_frame(&app, 220, 30),
            "validated by herdr config check;",
            "herdr config check",
            false,
        );
        app.modals.pop();
    }
}

/// A shared diagnostic names its command in backticks for the CLI; the TUI
/// toast built from it draws the command ACCENT, never bold, backticks gone.
#[test]
fn a_shared_diagnostic_toast_styles_its_backticked_command() {
    use crate::testutil::assert_prose_part;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = bare_app();
    app.toast(
        super::ToastKind::Danger,
        crate::format::login_expired(&crate::profile::ProfileName::from("work")).toast(),
    );
    let buf = prose_frame(&app, 120, 30);
    assert_prose_part(&buf, "run clauth login work", "clauth login work", false);
    assert!(
        crate::testutil::needle_cells(&buf, "`").is_none(),
        "no literal backtick reaches the toast"
    );
}

/// The capture row's refusal names the owner's switch command: ACCENT, never
/// bold, in its toast.
#[test]
fn the_capture_refusal_styles_the_owners_switch_command() {
    use crate::profile::{AppConfig, AppState, Profile, save_profile};
    use crate::testutil::assert_prose_part;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut play = Profile::new("play".to_string(), None, None);
    play.credentials = Some(creds_ra("rt-play", "at-play"));
    save_profile(&play).expect("save play");
    write_live_creds(&creds_ra("rt-play", "at-play"));
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec!["play".into()],
            ..AppState::default()
        },
        profiles: vec![play],
    });
    super::run_config_row(&mut app, super::ConfigRow::CaptureLogin);
    assert!(
        app.toasts.back().is_some_and(|t| t
            .body
            .starts_with("these credentials already belong to 'play'")),
        "the capture refuses an owned login: {:?}",
        app.toasts.back().map(|t| &t.body)
    );
    assert_prose_part(
        &prose_frame(&app, 120, 30),
        "switch to it with: clauth play",
        "clauth play",
        false,
    );
}

/// The move and both admin edits keep the cause chain too: with the adopted
/// config gone, each names the path it failed to read and, under it, the OS's
/// own reason.
#[test]
fn a_failed_move_or_admin_edit_keeps_the_cause_under_its_context() {
    let home = crate::testutil::HomeSandbox::new();
    let record = crate::testutil::write_adopted_record(&home, false, "[server]\n");
    std::fs::remove_file(record.config()).unwrap();
    let cause = std::fs::read_to_string(record.config()).expect_err("removed");
    let expected = format!("failed to read {}: {cause}", record.config().display());
    let plan = crate::gateway::StoreMovePlan {
        moved: Vec::new(),
        kept: Vec::new(),
    };
    match super::run_move(&record, &plan) {
        super::ShuntActionOutcome::MoveFailed { error } => assert_eq!(error, expected),
        other => panic!("expected the move to fail, got {other:?}"),
    }
    for step in [
        crate::gateway::AdminNeed::WriteKey,
        crate::gateway::AdminNeed::AdminTable,
    ] {
        match super::run_admin_edit(&record, step) {
            super::ShuntActionOutcome::AdminFailed { error } => {
                assert_eq!(error, expected, "{step:?}")
            }
            other => panic!("expected {step:?} to fail, got {other:?}"),
        }
    }
}

// ── #104: startup reconcile flags a current refreshless bearer ──────────────

/// #104: `reconcile_startup` compares the live login against the profile's
/// clauth-private usage pair (`credentials_diverged`), which requires the
/// refresh token to match. A rolling-token profile's live slot holds the
/// refreshless bearer, so the current bearer still reads as diverged and every
/// launch applies `default_divergence` (discard confirm) or flashes the banner.
/// The live login must be judged against what a switch installs — the
/// sidecar — where an access-token match is no divergence at all.
#[test]
fn repro_104_startup_reconcile_accepts_a_current_rolling_bearer() {
    use crate::profile::{
        AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, ProfileName, save_profile,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let name = "repro-104-rolling";
    let pair = OAuthToken {
        access_token: "at-chain".to_string(),
        refresh_token: Some("rt-chain".to_string()),
        expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
        scopes: Some(vec!["user:profile".into(), "user:inference".into()]),
        subscription_type: Some("max".into()),
        ..OAuthToken::default_extra()
    };
    let mut profile = Profile::new(name.to_string(), None, None);
    profile.rolling_token = true;
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(pair.clone()),
    });
    save_profile(&profile).expect("save profile");
    // The sidecar is the chain's refreshless projection: the SAME access token.
    crate::claude::stamp_rolling_token(&ProfileName::from(name), &pair).expect("stamp");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    // Typed fixture: no added line spells a JSON credential key (the
    // pre-commit secret scan refuses that shape outright).
    let live = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-chain".to_string(),
            refresh_token: None,
            expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
            scopes: Some(vec!["user:profile".into(), "user:inference".into()]),
            subscription_type: Some("max".into()),
            ..OAuthToken::default_extra()
        }),
    };
    std::fs::write(
        claude_dir.join(".credentials.json"),
        serde_json::to_vec(&live).expect("ser"),
    )
    .expect("write live");
    let state = AppState {
        active_profile: Some(ProfileName::from(name)),
        profiles: vec![ProfileName::from(name)],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let mut app = App::new(AppConfig {
        state,
        profiles: vec![profile],
    });
    super::reconcile_startup(&mut app);
    let signal = app.startup_results.try_recv();
    assert!(
        matches!(signal, Ok(crate::usage::StartupSignal::ReconcileDone)),
        "a live slot holding the CURRENT rolling bearer is not a divergence (#104): {signal:?}"
    );
}

/// The same class, wider: a static-mint profile's live slot holds the mint, a
/// DIFFERENT token than the clauth-private usage pair. The pair-compare reads
/// that as a divergence on every launch too; against the install source it is
/// not one.
#[test]
fn repro_104_startup_reconcile_accepts_a_current_static_mint() {
    use crate::profile::{
        AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, ProfileName, save_profile,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let name = "repro-104-static";
    let pair = OAuthToken {
        access_token: "at-chain".to_string(),
        refresh_token: Some("rt-chain".to_string()),
        expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
        scopes: Some(vec!["user:profile".into(), "user:inference".into()]),
        subscription_type: Some("max".into()),
        ..OAuthToken::default_extra()
    };
    let mut profile = Profile::new(name.to_string(), None, None);
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(pair),
    });
    save_profile(&profile).expect("save profile");
    crate::claude::write_session_token(
        &ProfileName::from(name),
        "setup-mint-fixture",
        crate::usage::now_ms() as i64,
    )
    .expect("mint");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "setup-mint-fixture".to_string(),
            refresh_token: None,
            expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
            scopes: None,
            subscription_type: None,
            ..OAuthToken::default_extra()
        }),
    };
    std::fs::write(
        claude_dir.join(".credentials.json"),
        serde_json::to_vec(&live).expect("ser"),
    )
    .expect("write live");
    let state = AppState {
        active_profile: Some(ProfileName::from(name)),
        profiles: vec![ProfileName::from(name)],
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
    let mut app = App::new(AppConfig {
        state,
        profiles: vec![profile],
    });
    super::reconcile_startup(&mut app);
    let signal = app.startup_results.try_recv();
    assert!(
        matches!(signal, Ok(crate::usage::StartupSignal::ReconcileDone)),
        "a live slot holding the current static mint is not a divergence: {signal:?}"
    );
}

// ── Setup `alert at` row (bell_threshold) ───────────────────────────────────

/// An OAuth account "acct" persisted to disk, the Setup detail pane open on it
/// and its cursor on the `alert at` row.
fn alert_app(threshold: Option<f64>) -> App {
    use super::{ConfigRow, Tab, config_rows, enter_config_detail};
    let name = crate::profile::ProfileName::from("acct");
    let mut profile = crate::testutil::blank_profile(&name);
    profile.bell_threshold = threshold;
    crate::profile::save_profile(&profile).expect("persist acct");
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec![name],
        ..crate::profile::AppState::default()
    })
    .expect("persist state");
    let mut app = app_with(vec![profile]);
    app.tab = Tab::Setup;
    app.profile_cursor = 0;
    enter_config_detail(&mut app);
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::BellThreshold)
        .expect("the alert row is listed");
    app
}

fn saved_bell(app: &App) -> (Option<f64>, Option<f64>) {
    let name = crate::profile::ProfileName::from("acct");
    let memory = app.config().find(&name).and_then(|p| p.bell_threshold);
    let disk = crate::profile::load_profile(&name)
        .expect("reload acct")
        .bell_threshold;
    (memory, disk)
}

fn cursor_row(app: &App) -> super::ConfigRow {
    super::config_rows(app)[app.config_action_cursor]
}

fn type_alert(app: &mut App, value: &str) {
    super::handle_key(app, key(KeyCode::Enter));
    for _ in 0..8 {
        super::handle_key(app, key(KeyCode::Backspace));
    }
    for c in value.chars() {
        super::handle_key(app, key(KeyCode::Char(c)));
    }
    super::handle_key(app, key(KeyCode::Enter));
}

#[test]
fn the_alert_row_follows_auto_start_on_oauth_and_name_on_api_accounts() {
    use super::{ConfigRow, config_rows};
    let _home = crate::testutil::HomeSandbox::new();
    let oauth = alert_app(None);
    assert_eq!(
        config_rows(&oauth)[..3],
        [
            ConfigRow::Name,
            ConfigRow::AutoStart,
            ConfigRow::BellThreshold
        ]
    );

    let mut api = app_with(vec![crate::profile::Profile::new(
        "api".to_string(),
        Some("https://api.test".to_string()),
        Some("k".to_string()),
    )]);
    api.profile_cursor = 0;
    super::enter_config_detail(&mut api);
    assert_eq!(
        config_rows(&api)[..3],
        [
            ConfigRow::Name,
            ConfigRow::BellThreshold,
            ConfigRow::BaseUrl
        ]
    );

    let mut new_form = app_with(vec![]);
    new_form.profile_cursor = 0;
    assert!(
        !config_rows(&new_form).contains(&ConfigRow::BellThreshold),
        "the + new form has no account to ring for yet"
    );
}

#[test]
fn a_typed_alert_saves_to_disk_and_an_empty_one_turns_it_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(None);
    type_alert(&mut app, "90");
    assert_eq!(saved_bell(&app), (Some(90.0), Some(90.0)));
    assert_eq!(cursor_row(&app), super::ConfigRow::BellThreshold);
    assert_eq!(app.config_draft.as_ref().unwrap().active, None);
    assert_eq!(
        app.config_draft.as_ref().unwrap().bell_threshold.value,
        "90"
    );

    type_alert(&mut app, "");
    assert_eq!(saved_bell(&app), (None, None));
    assert_eq!(cursor_row(&app), super::ConfigRow::BellThreshold);
}

#[test]
fn an_out_of_range_alert_is_refused_and_the_editor_stays_open() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(Some(80.0));
    for raw in ["101", "-1", "abc", "NaN"] {
        type_alert(&mut app, raw);
        assert_eq!(saved_bell(&app), (Some(80.0), Some(80.0)), "{raw:?}");
        assert_eq!(
            app.config_draft.as_ref().unwrap().active,
            Some(super::ConfigRow::BellThreshold),
            "{raw:?} keeps the editor open"
        );
        super::handle_key(&mut app, key(KeyCode::Esc));
    }
    assert_eq!(
        app.config_draft.as_ref().unwrap().bell_threshold.value,
        "80"
    );
}

#[test]
fn plus_and_minus_step_the_alert_by_five_from_95_when_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(None);
    let mut seen = Vec::new();
    for c in ['+', '+', '+', '-', '-'] {
        super::handle_key(&mut app, key(KeyCode::Char(c)));
        seen.push(saved_bell(&app));
    }
    assert_eq!(
        seen,
        [
            (Some(95.0), Some(95.0)),
            (Some(100.0), Some(100.0)),
            (Some(100.0), Some(100.0)),
            (Some(95.0), Some(95.0)),
            (Some(90.0), Some(90.0)),
        ]
    );
    assert_eq!(cursor_row(&app), super::ConfigRow::BellThreshold);
    assert_eq!(
        app.config_draft.as_ref().unwrap().bell_threshold.value,
        "90"
    );
}

#[test]
fn minus_stops_at_zero() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(Some(3.0));
    super::handle_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(saved_bell(&app), (Some(0.0), Some(0.0)));
}

#[test]
fn plus_on_another_row_leaves_the_alert_alone() {
    use super::{ConfigRow, config_rows};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(Some(80.0));
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::AutoStart)
        .unwrap();
    super::handle_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(saved_bell(&app), (Some(80.0), Some(80.0)));
}

#[test]
fn a_failed_alert_save_rolls_memory_back_and_says_so() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(Some(80.0));
    let config_toml = crate::profile::profile_dir(&crate::profile::ProfileName::from("acct"))
        .unwrap()
        .join("config.toml");
    std::fs::remove_file(&config_toml).unwrap();
    std::fs::create_dir(&config_toml).unwrap();

    super::handle_key(&mut app, key(KeyCode::Char('+')));

    let name = crate::profile::ProfileName::from("acct");
    assert_eq!(
        app.config().find(&name).and_then(|p| p.bell_threshold),
        Some(80.0),
        "memory keeps what disk still holds"
    );
    let last = app.toasts.back().expect("a toast");
    assert_eq!(last.kind, super::ToastKind::Danger);
    assert!(
        last.body.starts_with("alert update failed\n"),
        "got {:?}",
        last.body
    );
    assert_eq!(
        app.config_draft.as_ref().unwrap().bell_threshold.value,
        "80"
    );
}

#[test]
fn a_typed_alert_reseeds_the_buffer_from_what_was_saved() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(None);
    for (raw, saved, shown) in [
        ("1e2", 100.0, "100"),
        ("97.5", 97.5, "97.5"),
        ("-0", 0.0, "0"),
        (" 42 ", 42.0, "42"),
    ] {
        type_alert(&mut app, raw);
        assert_eq!(saved_bell(&app), (Some(saved), Some(saved)), "{raw:?}");
        assert_eq!(
            app.config_draft.as_ref().unwrap().bell_threshold.value,
            shown,
            "{raw:?}"
        );
        assert_eq!(app.config_draft.as_ref().unwrap().active, None, "{raw:?}");
    }
}

#[test]
fn minus_from_off_also_starts_at_95_and_the_shifted_keys_step_too() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(None);
    let mut seen = Vec::new();
    for c in ['-', '=', '_'] {
        super::handle_key(&mut app, key(KeyCode::Char(c)));
        seen.push(saved_bell(&app).1);
    }
    assert_eq!(seen, [Some(95.0), Some(100.0), Some(95.0)]);
}

#[test]
fn a_step_lands_on_the_hundredths_a_typed_value_carries() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(Some(5.01));
    super::handle_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(saved_bell(&app), (Some(0.01), Some(0.01)));
    assert_eq!(
        app.config_draft.as_ref().unwrap().bell_threshold.value,
        "0.01"
    );
}

#[test]
fn opening_the_alert_editor_reseeds_from_a_reloaded_value() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(Some(80.0));
    let name = crate::profile::ProfileName::from("acct");
    app.config().find_mut(&name).unwrap().bell_threshold = Some(50.0);
    super::handle_key(&mut app, key(KeyCode::Enter));
    let d = app.config_draft.as_ref().unwrap();
    assert_eq!(d.active, Some(super::ConfigRow::BellThreshold));
    assert_eq!(
        d.bell_threshold.value, "50",
        "the editor opens on the value a reload brought in"
    );
}

#[test]
fn an_alert_commit_after_the_account_left_ends_the_edit() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(Some(80.0));
    super::handle_key(&mut app, key(KeyCode::Enter));
    app.config().profiles.clear();
    for _ in 0..4 {
        super::handle_key(&mut app, key(KeyCode::Backspace));
    }
    super::handle_key(&mut app, key(KeyCode::Char('9')));
    super::handle_key(&mut app, key(KeyCode::Enter));
    assert_eq!(app.config_draft.as_ref().map(|d| d.active), Some(None));
}

#[test]
fn the_open_alert_editor_shows_its_range_line_in_the_pane() {
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = alert_app(Some(80.0));
    let draw = |app: &App| {
        let mut term = Terminal::new(TestBackend::new(110, 30)).unwrap();
        term.draw(|f| crate::tui::render::draw(f, app)).unwrap();
        crate::testutil::buffer_rows(term.backend().buffer())
    };
    let range = "└ 0-100 % · empty turns it off";
    assert!(
        !draw(&app).iter().any(|r| r.contains(range)),
        "no range line at rest"
    );
    super::handle_key(&mut app, key(KeyCode::Enter));
    let rows = draw(&app);
    let field = rows.iter().position(|r| r.contains("alert at")).unwrap();
    assert!(rows[field + 1].contains(range), "{}", rows.join("\n"));
}
