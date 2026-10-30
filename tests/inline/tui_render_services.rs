//! Services-tab render tests. Each row is a dot + label in the selector; the
//! verdict lives in the detail pane. The `herdr` row's verdict logic is
//! unit-tested in `tests/inline/tui_app.rs`; these pin the render per drift
//! state. The delegates detail shows the job list; its rows, the selected
//! job's expansion and the empty state are pinned here too.

use crate::herdr::{ConfigStatus, HerdrProbe, RegistryEntry, SidebarState};
use crate::mcp::jobs::{self, JobRecord, JobState, RecordKind, RunningSpec};
use crate::profile::{AppConfig, AppState};
use crate::testutil::{EYEBROW_ACTIVE, EYEBROW_AT_REST};
use crate::tui::app::{App, Check, Health, Problem, ServiceFix, delegates_check, herdr_check};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::PathBuf;

const W: u16 = 100;
const H: u16 = 24;

fn entry(enabled: bool, min: Option<&str>, warnings: Vec<&str>) -> RegistryEntry {
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

fn probe(version: Option<&str>, entry: Option<RegistryEntry>, error: Option<&str>) -> HerdrProbe {
    HerdrProbe {
        version: version.map(str::to_string),
        entry,
        config_path: Some(PathBuf::from("/tmp/herdr/config.toml")),
        error: error.map(str::to_string),
    }
}

fn config(parsed: bool, key: Option<&str>, sidebar: SidebarState) -> ConfigStatus {
    ConfigStatus {
        parsed,
        bound_key: key.map(str::to_string),
        sidebar,
    }
}

fn healthy_probe() -> HerdrProbe {
    probe(
        Some("0.8.0"),
        Some(entry(true, Some("0.8.0"), vec![])),
        None,
    )
}

fn healthy_config() -> ConfigStatus {
    config(true, Some("prefix+a"), SidebarState::Templated)
}

/// A `plugin` row with two fixable problems (install, then wire) and the folded
/// readout in the builder's order, built directly so the render pins never
/// touch the FS probes.
fn plugin_check_with_problems() -> Check {
    Check {
        label: "plugin",
        health: Health::Warn,
        detail: vec![
            "installed: no (marketplace known)".to_string(),
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
            "path: /usr/local/bin/clauth".to_string(),
            "data: /home/u/.clauth".to_string(),
        ],
        fix: Some(ServiceFix::InstallPlugin),
        problems: vec![
            Problem {
                line: 2,
                fix: ServiceFix::InstallPlugin,
            },
            Problem {
                line: 7,
                fix: ServiceFix::WireMcpServers,
            },
        ],
        shunt_focus: Vec::new(),
        shunt_action_start: None,
    }
}

/// A shunt slot for the render pins: the fields the pick lists, each optional.
fn shunt_slot(state: crate::daemon::gateway::GatewayState) -> crate::daemon::gateway::GatewaySlot {
    crate::daemon::gateway::GatewaySlot {
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

/// A shunt slot with every optional field set, for the detail render pin.
fn shunt_slot_full() -> crate::daemon::gateway::GatewaySlot {
    crate::daemon::gateway::GatewaySlot {
        state: crate::daemon::gateway::GatewayState::Healthy,
        config: Some("/home/u/.clauth/gateway.toml".to_string()),
        binary: Some("/usr/local/bin/shunt".to_string()),
        port: Some(3001),
        pid: Some(4242),
        version: Some("0.49.1".to_string()),
        answerer: None,
        floor: "0.48.0".to_string(),
        restarts: 2,
        last_exit: Some(crate::daemon::gateway::ExitReport {
            code: Some(1),
            signal: None,
        }),
        reason: Some("port busy".to_string()),
        since: Some("2026-09-29T00:00:00Z".to_string()),
    }
}

fn app_with(check: Check) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = crate::tui::app::Tab::Services;
    app.services.checks = vec![check];
    app.services.cursor = 0;
    app
}

fn app_with_checks(checks: Vec<Check>) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = crate::tui::app::Tab::Services;
    app.services.checks = checks;
    app.services.cursor = 0;
    app
}

fn render(app: &App) -> (Vec<String>, ratatui::buffer::Buffer) {
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app, &[])).unwrap();
    let buf = term.backend().buffer().clone();
    (crate::testutil::buffer_rows(&buf), buf)
}

/// The whole frame (header + body + footer), for the footer-hint pins.
fn dump_full(app: &App) -> String {
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    term.draw(|f| super::super::draw(f, app)).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer()).join("\n")
}

/// The detail pane's content rows, trimmed of the selector pane, the border and
/// the pane padding — one `key value` line per row, blank padding dropped.
fn detail_rows(rows: &[String]) -> Vec<String> {
    rows.iter()
        .filter_map(|r| r.split("││").nth(1))
        .map(|r| r.trim_matches('│').trim().to_string())
        .filter(|r| !r.is_empty())
        .collect()
}

/// The dot carries the verdict hue and the selector row carries no `[f]` cue —
/// dots only. `expected` is the health the state should render.
fn assert_dot(check: &Check, expected: Health) {
    let app = app_with(check.clone());
    let (rows, buf) = render(&app);
    let row_idx = rows
        .iter()
        .position(|r| r.contains('●'))
        .unwrap_or_else(|| panic!("no selector row:\n{}", rows.join("\n")));
    let row = &rows[row_idx];

    // Buffer COLUMN, not byte offset — the caret and dot are multi-byte.
    let byte = row.find('●').expect("dot renders");
    let col = row[..byte].chars().count();
    // Map the verdict to its theme hue here, NOT via `health_color`, so a
    // regression in the mapping itself reddens the test instead of moving both
    // sides in lockstep.
    let want = match expected {
        Health::Ok => super::theme::success_color(),
        Health::Warn => super::theme::warning_color(),
        Health::Danger => super::theme::danger_color(),
        Health::Idle => super::theme::text_dim_color(),
    };
    assert_eq!(
        buf.content[row_idx * W as usize + col].fg,
        want,
        "dot hue for {:?}:\n{}",
        expected,
        rows.join("\n")
    );
    assert!(
        !row.contains("[f]"),
        "a selector row is dots only, no `[f]` cue:\n{row}"
    );
}

// ── selector rows ──────────────────────────────────────────────────────────────

/// The title spinner shows while either Services probe runs on its worker,
/// and not otherwise.
#[test]
fn the_title_spinner_shows_while_a_services_probe_runs() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(plugin_check_with_problems());
    let spun = format!(" SERVICES {} ", super::spinner_frame(app.tick_count));
    let (rows, _) = render(&app);
    assert!(!rows[0].contains(&spun), "no spinner at rest:\n{}", rows[0]);
    app.services.herdr_probe.running = true;
    let (rows, _) = render(&app);
    assert!(
        rows[0].contains(&spun),
        "the spinner while herdr probes:\n{}",
        rows[0]
    );
    app.services.herdr_probe.running = false;
    app.services.standalone_probe.running = true;
    let (rows, _) = render(&app);
    assert!(
        rows[0].contains(&spun),
        "the spinner while the standalone shunt probe runs:\n{}",
        rows[0]
    );
    app.services.standalone_probe.running = false;
    for (name, set) in [
        ("move plan", set_move_plan_running as fn(&mut App)),
        ("pool read", set_pool_running as fn(&mut App)),
        ("shunt action", set_action_running as fn(&mut App)),
    ] {
        set(&mut app);
        let (rows, _) = render(&app);
        assert!(
            rows[0].contains(&spun),
            "the spinner while the {name} runs:\n{}",
            rows[0]
        );
    }
}

fn set_move_plan_running(app: &mut App) {
    app.services.move_plan_probe.running = true;
}
fn set_pool_running(app: &mut App) {
    app.services.pool_probe.running = true;
}
fn set_action_running(app: &mut App) {
    app.services.shunt_action.running = true;
}

/// The four service rows render as dot + label, in order, with no fix cue on
/// any row.
#[test]
fn the_selector_lists_each_service_row_dot_and_label_only() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_checks(vec![
        crate::tui::app::shunt_check(
            &shunt_slot(crate::daemon::gateway::GatewayState::Absent),
            false,
            None,
        ),
        delegates_check(&[], &[]),
        plugin_check_with_problems(),
        herdr_check(&healthy_probe(), Some(&healthy_config())),
    ]);
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    for label in ["shunt", "delegates", "plugin", "herdr"] {
        assert!(
            screen.contains(&format!("● {label}")),
            "the `{label}` row renders dot + label:\n{screen}"
        );
    }
    assert!(
        !screen.contains("[f]"),
        "no selector row carries a fix cue; the fix lives in the detail:\n{screen}"
    );
}

/// The shunt dot buckets each state into its class (green / amber / red / dim).
#[test]
fn the_shunt_dot_maps_each_state_to_its_class() {
    use crate::daemon::gateway::GatewayState as S;
    let _home = crate::testutil::HomeSandbox::new();
    for (state, want) in [
        (S::Healthy, Health::Ok),
        (S::Starting, Health::Warn),
        (S::Unhealthy, Health::Warn),
        (S::Restarting, Health::Warn),
        (S::Stopping, Health::Warn),
        (S::Misconfigured, Health::Danger),
        (S::BinaryMissing, Health::Danger),
        (S::Foreign, Health::Danger),
        (S::YamlRefused, Health::Danger),
        (S::BelowFloor, Health::Danger),
        (S::NoConfig, Health::Danger),
        (S::Absent, Health::Idle),
        (S::Disabled, Health::Idle),
        (S::Unobserved, Health::Idle),
    ] {
        assert_dot(
            &crate::tui::app::shunt_check(&shunt_slot(state), false, None),
            want,
        );
    }
}

/// The shunt detail renders every set field in order, pinned by equality on the
/// detail pane's content rows (key column 11 cells: widest key `last exit` +
/// the 2-space gap).
#[test]
fn the_shunt_detail_renders_every_set_field() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with(crate::tui::app::shunt_check(&shunt_slot_full(), true, None));
    let (rows, _) = render(&app);
    assert_eq!(
        detail_rows(&rows),
        vec![
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
        ],
        "every set field renders in order"
    );
}

/// The card's `config` readout over a real adoption names the plain path,
/// never the `\\?\C:\…` verbatim form Windows canonicalizes to.
#[test]
fn the_shunt_card_names_the_plain_config_path() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = home.home().join("etc");
    std::fs::create_dir_all(&dir).expect("etc");
    let config = dir.join("shunt.toml");
    std::fs::write(&config, "[server]\n").expect("config");
    let record = crate::gateway::GatewayRecord::new(config.clone()).expect("adoptable");
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record);
        Ok(())
    })
    .expect("save");
    let plain = crate::testutil::plain_path(&config);

    let slot = crate::daemon::gateway::unsupervised_slot();
    let card = crate::tui::app::shunt_check(&slot, false, None);
    let line = format!("config: {}", plain.display());
    assert!(
        card.detail.contains(&line),
        "the card reads {line:?}: {:?}",
        card.detail
    );
}

/// The untrusted `/health` version's planted `ESC [2J` renders as its visible
/// escape, pinned by equality on the rendered version row.
#[test]
fn the_shunt_detail_escapes_a_planted_screen_clear() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut slot = shunt_slot_full();
    slot.version = Some("0.49.1\u{1b}[2J".to_string());
    let app = app_with(crate::tui::app::shunt_check(&slot, true, None));
    let (rows, _) = render(&app);
    let detail = detail_rows(&rows);
    assert_eq!(
        detail.iter().find(|r| r.starts_with("version")),
        Some(&"version    0.49.1\\u{1b}[2J".to_string()),
        "the version renders escaped: {detail:?}"
    );
}

// ── footer hints ───────────────────────────────────────────────────────────────

/// Each Services focus state's whole footer hint list, pinned by equality: the
/// `↵ detail` gate on delegates, the list `f` verb, the plugin detail's
/// per-problem verb, and the herdr detail's options keys.
#[test]
fn the_services_footer_hints_pin_each_focus_state() {
    use crate::tui::app::ServicesFocus;
    let _home = crate::testutil::HomeSandbox::new();
    let hints = |app: &App| super::super::footer::services_hints(app);

    // List on a job-less delegates row: no `↵ detail`, no `f`.
    let app = app_with_checks(vec![
        delegates_check(&[], &[]),
        plugin_check_with_problems(),
    ]);
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "a job-less delegates row binds no `↵` beyond the shared keys"
    );

    // List on a delegates row WITH jobs: `↵ detail` appears, no `f`.
    let app = app_with_delegates(seed_walk_jobs(3));
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "row"),
            ("↵", "detail"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "a delegates row with jobs advertises `↵ detail`"
    );

    // Descended into the delegates detail (overflowing): `↑↓ row` +
    // `pgup/pgdn read` + the shared keys.
    let mut app = app_with_delegates(seed_walk_jobs(22));
    app.services.focus = ServicesFocus::Detail;
    render(&app); // publish the geometry the page hint derives from
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "row"),
            ("pgup/pgdn", "read"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "the descended delegates detail advertises its walking keys"
    );

    // List on plugin (with a fix): `↵ detail` and the list `f` walk.
    let app = app_with(plugin_check_with_problems());
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "row"),
            ("↵", "detail"),
            ("r", "refresh"),
            ("f", "next fix"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "the list focus advertises the walk to the first fix line"
    );

    // List on herdr (no fix): `↵ detail`, no `f`.
    let app = app_with(herdr_check(&healthy_probe(), Some(&healthy_config())));
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "row"),
            ("↵", "detail"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "a healthy herdr row offers no fix verb"
    );

    // Plugin detail, focused problem 0 and 1: the verb follows the focus.
    let mut app = app_with(plugin_check_with_problems());
    app.services.focus = ServicesFocus::Detail;
    app.services.problem_cursor = 0;
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "problem"),
            ("r", "refresh"),
            ("↵", "install plugin"),
            ("f", "next fix"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "the install problem names its verb"
    );
    app.services.problem_cursor = 1;
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "problem"),
            ("r", "refresh"),
            ("↵", "wire mcp server"),
            ("f", "next fix"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "the wire problem names its verb"
    );

    // Herdr detail: the options row keys, no `f` on a healthy check.
    let app = herdr_options_app(healthy_config());
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "row"),
            ("space/↵", "cycle / toggle"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "the herdr options detail advertises its own keys"
    );
}

/// Q8/Q10: while a shunt action runs, the footer drops the focused fix line's
/// `↵` verb, derived from the same `running` gate the handler reads; `f next
/// fix` stays, since walking focus starts no action.
#[test]
fn a_shunt_action_running_drops_the_footer_fix_verb() {
    use crate::tui::app::ServicesFocus;
    let home = crate::testutil::HomeSandbox::new();
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let mut app = shunt_card_app(
        slot,
        false,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        None,
        None,
    );
    app.services.shunt_action.running = true;

    // List focus: `f` only walks, which the running action does not gate.
    let hints = super::super::footer::services_hints(&app);
    assert!(
        hints.contains(&("f", "next fix")),
        "the list keeps the walk while a shunt action runs: {hints:?}"
    );

    // Detail focus: the focused `f add admin key` line drops its `↵` verb.
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(crate::tui::app::ShuntFocusLine::Admin));
    let hints = super::super::footer::services_hints(&app);
    assert!(
        !hints.iter().any(|(_, v)| *v == "add admin key"),
        "no detail `↵` verb while a shunt action runs: {hints:?}"
    );
}

/// The plugin detail folds the four readouts and renders each fix as a dim
/// `f  <verb>` line under its problem — never a bracketed `[f]` cue. The
/// content rows pin by equality (key column 12 cells: widest key `mcp source`
/// + the 2-space gap; blank separators dropped by `detail_rows`).
#[test]
fn the_plugin_detail_folds_the_readouts_and_renders_bare_f_lines() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = app_with(plugin_check_with_problems());
    let (rows, buf) = render(&app);
    let screen = rows.join("\n");
    assert_eq!(
        detail_rows(&rows),
        vec![
            "installed   no (marketplace known)",
            "installs at user scope",
            "f  install plugin",
            "mcp entry   not registered",
            "mcp source  none",
            "writes the clauth entry into ~/.claude.json",
            "f  wire mcp server",
            "claude      press r to probe",
            "path        /usr/local/bin/clauth",
            "data        /home/u/.clauth",
        ],
        "every readout in order, by equality"
    );
    assert!(
        !screen.contains("[f]"),
        "the bracketed `[f]` anti-pattern is gone:\n{screen}"
    );

    // An unfocused fix line: `f` is a key in prose (ACCENT + bold, cloudy
    // 2026-10-02), the verb dim, pinned off the styled buffer.
    crate::testutil::assert_prose_part(&buf, "f  wire mcp server", "f", true);
    let verb = crate::testutil::needle_cells(&buf, "wire mcp server").expect("verb renders");
    assert!(
        verb.iter()
            .filter(|c| c.symbol() != " ")
            .all(|c| c.fg == super::theme::text_dim_color()),
        "the verb stays dim:\n{screen}"
    );
}

// ── completions fix ──────────────────────────────────────────────────────────

/// Plant completion bytes at the path `install` wrote, so the test never
/// re-types the writer's location.
fn plant_completions(home: &crate::testutil::HomeSandbox, shell: &str, bytes: &str) {
    crate::completions::install(Some(shell)).expect("install the completion script");
    let path = match shell {
        "fish" => home
            .home()
            .join(".config")
            .join("fish")
            .join("completions")
            .join("clauth.fish"),
        _ => home
            .home()
            .join(".clauth")
            .join("completions")
            .join(format!("clauth.{shell}")),
    };
    std::fs::write(path, bytes).expect("plant the completion bytes");
}

/// The Services `plugin` detail, built by the production probe over the
/// sandbox `$SHELL` and its installed script.
fn plugin_detail_over_completions() -> Check {
    crate::tui::app::plugin_check(
        Some(std::path::Path::new("/usr/bin/clauth")),
        None,
        None,
        crate::completions::detected_installed_script(),
    )
}

/// The readouts every plugin detail carries, with the key column at `pad`
/// (the display width before each value).
fn plugin_base_rows(pad: usize) -> Vec<String> {
    let spaces = |key_w: usize| " ".repeat(pad.saturating_sub(key_w) + 2);
    vec![
        format!("installed{}no (marketplace unknown)", spaces(9)),
        "installs at user scope".to_string(),
        "f  install plugin".to_string(),
        format!("mcp entry{}not registered", spaces(9)),
        format!("mcp source{}none", spaces(10)),
        "writes the clauth entry into ~/.claude.json".to_string(),
        "f  wire mcp server".to_string(),
        format!("claude{}press r to probe", spaces(6)),
        format!("path{}/usr/bin/clauth", spaces(4)),
        "data".to_string(),
    ]
}

/// The plugin detail's rows with the `data` row cut to its key: its value is
/// the sandbox home's path, which the pane truncates at a width that depends on
/// how long the temp dir's path is.
fn plugin_rows(rows: &[String]) -> Vec<String> {
    detail_rows(rows)
        .into_iter()
        .map(|row| {
            if row.starts_with("data ") {
                "data".to_string()
            } else {
                row
            }
        })
        .collect()
}

/// A stale script names the problem and its `f  update completions` line; a
/// current one shows neither. Pinned by equality off the rendered rows.
#[test]
fn the_plugin_detail_offers_update_completions_for_a_stale_script() {
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let _shell =
        crate::testutil::EnvPin::new(&home, &[("SHELL", Some(std::ffi::OsStr::new("/bin/bash")))]);

    plant_completions(&home, "bash", "an older script\n");
    let stale = app_with(plugin_detail_over_completions());
    let (rows, _) = render(&stale);
    let mut expected = plugin_base_rows(11);
    expected.extend([
        "completions  stale (bash)".to_string(),
        "bash script differs from this clauth's".to_string(),
        "f  update completions".to_string(),
    ]);
    assert_eq!(
        plugin_rows(&rows),
        expected,
        "a stale script names the problem and its update fix, by equality"
    );

    crate::completions::install(Some("bash")).expect("rewrite the script");
    let current = app_with(plugin_detail_over_completions());
    let (rows, _) = render(&current);
    assert_eq!(
        plugin_rows(&rows),
        plugin_base_rows(10),
        "a current script shows no completions line and no fix, by equality"
    );
}

/// No script with a known shell offers `f  install completions`; an unknown
/// shell shows nothing, since clauth ships no script to compare against.
#[test]
fn the_plugin_detail_offers_install_completions_only_for_a_known_shell() {
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);

    let _bash =
        crate::testutil::EnvPin::new(&home, &[("SHELL", Some(std::ffi::OsStr::new("/bin/bash")))]);
    let missing = app_with(plugin_detail_over_completions());
    let (rows, _) = render(&missing);
    let mut expected = plugin_base_rows(11);
    expected.extend([
        "completions  missing (bash)".to_string(),
        "bash script not installed".to_string(),
        "f  install completions".to_string(),
    ]);
    assert_eq!(
        plugin_rows(&rows),
        expected,
        "a missing script names the problem and its install fix, by equality"
    );
    drop(_bash);

    let _tcsh =
        crate::testutil::EnvPin::new(&home, &[("SHELL", Some(std::ffi::OsStr::new("/bin/tcsh")))]);
    let unknown = app_with(plugin_detail_over_completions());
    let (rows, _) = render(&unknown);
    assert_eq!(
        plugin_rows(&rows),
        plugin_base_rows(10),
        "an unknown shell renders the card without any completions row"
    );
    drop(_tcsh);

    // An unset `$SHELL` takes the same no-line branch.
    let _unset = crate::testutil::EnvPin::new(&home, &[("SHELL", None)]);
    let absent = app_with(plugin_detail_over_completions());
    let (rows, _) = render(&absent);
    let detail = plugin_rows(&rows);
    assert!(
        detail.iter().any(|l| l.starts_with("data")),
        "the plugin detail rendered: {detail:?}"
    );
    assert_eq!(
        detail,
        plugin_base_rows(10),
        "an unset $SHELL renders the card without any completions row"
    );
}

/// A script an older clauth wrote (here: this binary's emission with every
/// `limit-reset` line gone, a verb it never knew) runs all the way to the
/// card's update line.
#[test]
fn a_verb_removed_script_reaches_the_cards_update_line() {
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let _shell =
        crate::testutil::EnvPin::new(&home, &[("SHELL", Some(std::ffi::OsStr::new("/bin/bash")))]);
    let older: String = crate::completions::emission("bash")
        .expect("bash emission")
        .lines()
        .filter(|line| !line.contains("limit-reset"))
        .map(|line| format!("{line}\n"))
        .collect();
    plant_completions(&home, "bash", &older);
    assert_eq!(
        crate::completions::script_state("bash").expect("a supported shell"),
        crate::completions::ScriptState::Stale,
        "fixture: the older script reads stale"
    );

    let app = app_with(plugin_detail_over_completions());
    let (rows, _) = render(&app);
    let detail = detail_rows(&rows);
    assert!(
        detail.iter().any(|l| l == "f  update completions"),
        "the older script's card offers the update: {detail:?}"
    );
}

/// A stale completion value warns; a missing one is an invitation, not a
/// problem state, and stays body text.
#[test]
fn the_completions_value_tones_only_a_stale_script() {
    let value_styles = |text: &str| {
        super::detail_line(text, 11, 80)
            .spans
            .iter()
            .skip(1)
            .map(|s| s.style)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        value_styles("completions: stale (bash)"),
        vec![super::theme::warning()],
        "a stale script warns"
    );
    assert_eq!(
        value_styles("completions: missing (bash)"),
        vec![super::theme::body()],
        "a missing script is no problem state"
    );
}

/// Descending into the plugin detail, ↑↓ walks the fixable problems and the
/// focused one takes the caret; `↵` opens the focused problem's confirm.
#[test]
fn the_plugin_detail_walks_problems_and_enter_opens_the_focused_fix() {
    use crate::tui::app::{Modal, ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(plugin_check_with_problems());
    // Descend into the plugin detail.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);

    // First problem (install) is focused; the footer names its verb.
    let dump = dump_full(&app);
    assert!(
        dump.contains("↵ install plugin"),
        "the footer names the focused problem's verb:\n{dump}"
    );
    let (rows, buf) = render(&app);
    let screen = rows.join("\n");
    assert!(
        rows.iter()
            .any(|r| r.contains("❯") && r.contains("install plugin")),
        "the focused problem takes the caret:\n{screen}"
    );
    // The focused problem line takes the same hover tint a focused herdr
    // option takes: bg pinned off the styled buffer, not the glyph text.
    let caret_row = rows
        .iter()
        .position(|r| r.contains("❯") && r.contains("install plugin"))
        .expect("focused problem row");
    let caret_byte = rows[caret_row].find("f  install").expect("f glyph");
    let caret_col = rows[caret_row][..caret_byte].chars().count();
    assert_eq!(
        buf.content[caret_row * W as usize + caret_col].bg,
        super::theme::bg_hover(),
        "the focused problem line carries the hover tint:\n{screen}"
    );

    // `↵` on the focused problem opens the install confirm.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => assert!(
            matches!(
                state.on_confirm,
                crate::tui::app::ConfirmAction::InstallPlugin
            ),
            "↵ on the focused install problem runs the install"
        ),
        other => panic!("expected an install confirm, got {other:?}"),
    }
    app.modals.clear();

    // Walk to the second problem (wire); the footer and caret follow.
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    let dump = dump_full(&app);
    assert!(
        dump.contains("↵ wire mcp server"),
        "the footer follows the focus to the wire verb:\n{dump}"
    );
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    assert!(
        rows.iter()
            .any(|r| r.contains("❯") && r.contains("wire mcp server")),
        "the caret follows to the wire problem:\n{screen}"
    );

    // `↵` now opens the wire fix.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => assert!(
            matches!(
                state.on_confirm,
                crate::tui::app::ConfirmAction::WireMcpServers
            ),
            "↵ on the focused wire problem writes the entry"
        ),
        other => panic!("expected a wire confirm, got {other:?}"),
    }
}

/// The shunt card pages by the viewport minus one and re-picks the first
/// actionable line in view (or none) — the same rule a resize applies.
#[test]
fn shunt_card_pages_by_viewport_minus_one_and_resettles_focus() {
    use crate::tui::app::{Check, Health, ServiceFix, ServicesFocus, ShuntFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let check = Check {
        label: "shunt",
        health: Health::Ok,
        detail: (0..30).map(|i| format!("plan row {i:02}")).collect(),
        fix: None,
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 5,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Enabled { line: 9 },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    let _ = draw_frame(&app, 80, 14);
    // 30 lines at 80x14 leave an 8-line card: one page = 7 lines.
    assert_eq!(
        app.services.shunt_viewport.get(),
        8,
        "the card draws 8 lines"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(
        app.services.shunt_scroll.get(),
        7,
        "the card pages by viewport - 1 = 7"
    );
    let rows = draw_frame(&app, 80, 14);
    // At scroll 7 the first action line in view is `enabled` (row 9).
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(crate::tui::app::ShuntFocusLine::Enabled),
        "focus re-picks the first action line in view"
    );
    assert!(
        rows.join("\n").contains('❯'),
        "the focused action line carries the caret"
    );
    // F11: a page key that leaves the scroll unchanged (already at the end)
    // keeps the held focus instead of re-picking the first action line.
    app.services
        .shunt_scroll
        .set(app.services.detail_max_scroll.get() as usize);
    app.services
        .set_shunt_focus(Some(crate::tui::app::ShuntFocusLine::Move));
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(
        app.services.shunt_scroll.get(),
        app.services.detail_max_scroll.get() as usize,
        "the card is already at the bottom"
    );
    assert_eq!(
        app.services.shunt_focus_kind(),
        Some(crate::tui::app::ShuntFocusLine::Move),
        "the held focus stays when the page did not move"
    );
}

#[test]
fn shunt_page_keeps_held_focus_when_a_resize_fits_the_card() {
    use crate::tui::app::{Check, Health, ServiceFix, ServicesFocus, ShuntFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    let _home = crate::testutil::HomeSandbox::new();
    let check = Check {
        label: "shunt",
        health: Health::Ok,
        detail: (0..30).map(|i| format!("plan row {i:02}")).collect(),
        fix: None,
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 5,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Enabled { line: 9 },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    let mut term = Terminal::new(TestBackend::new(80, 14)).unwrap();
    term.draw(|f| super::super::draw(f, &app)).unwrap();
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    term.draw(|f| super::super::draw(f, &app)).unwrap();
    // ↓↓ lands focus on the `enabled` row, two lines into the card.
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    term.draw(|f| super::super::draw(f, &app)).unwrap();
    assert_eq!(
        app.services.shunt_focus_kind(),
        Some(crate::tui::app::ShuntFocusLine::Enabled),
        "two ↓ land on the enabled row"
    );
    // Grow the terminal so the card fits: the rendered offset clamps to 0 while
    // the stored scroll stays above the new bound.
    term.backend_mut().resize(80, 50);
    term.autoresize().unwrap();
    term.draw(|f| super::super::draw(f, &app)).unwrap();
    assert_eq!(
        app.services.detail_max_scroll.get(),
        0,
        "the grown card fits"
    );
    let rendered_before = app
        .services
        .shunt_scroll
        .get()
        .min(app.services.detail_max_scroll.get() as usize);
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(
        app.services.shunt_focus_kind(),
        Some(crate::tui::app::ShuntFocusLine::Enabled),
        "a page that moves nothing keeps the held focus"
    );
    assert_eq!(
        app.services
            .shunt_scroll
            .get()
            .min(app.services.detail_max_scroll.get() as usize),
        rendered_before,
        "the rendered offset stays put"
    );
    // PageUp twin: still nothing moves.
    handle_key(&mut app, crate::testutil::key(KeyCode::PageUp));
    assert_eq!(
        app.services.shunt_focus_kind(),
        Some(crate::tui::app::ShuntFocusLine::Enabled),
        "the PageUp twin keeps the held focus too"
    );
}

#[test]
fn plugin_diagnostics_and_fix_remain_reachable_after_page_scroll() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut check = plugin_check_with_problems();
    check
        .detail
        .extend((0..28).map(|i| format!("diagnostic line {i:02}")));
    let mut app = app_with(check);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    let first = draw_frame(&app, 80, 14);
    assert!(!first.join("\n").contains("diagnostic line 27"));
    let initial_offset = app.services.displayed_scroll.get();
    assert!(initial_offset <= app.services.detail_max_scroll.get());
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(
        app.services.detail_scroll,
        initial_offset
            .saturating_add(
                crate::tui::app::page_step(app.services.detail_viewport.get() as usize) as u16
            )
            .min(app.services.detail_max_scroll.get()),
        "paging starts at the visible problem, not a stale scroll field"
    );
    let _ = draw_frame(&app, 80, 14);
    let once = app.services.detail_scroll;
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(
        app.services.detail_scroll,
        once.saturating_add(
            crate::tui::app::page_step(app.services.detail_viewport.get() as usize) as u16
        )
        .min(app.services.detail_max_scroll.get()),
        "a second page press before redraw must move"
    );
    for _ in 0..5 {
        handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
        let _ = draw_frame(&app, 80, 14);
    }
    let last = draw_frame(&app, 80, 14);
    assert!(
        last.join("\n").contains("diagnostic line 27"),
        "last diagnostic must be readable"
    );
    assert!(
        app.services.focused_fix().is_none(),
        "an offscreen fix has no action target"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        app.modals.is_empty(),
        "offscreen fix must not open a confirmation"
    );
    // `f` from the paged view walks to the first fix line at or below the
    // view's top, wrapping to the first one when every fix sits above it.
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(
        app.services.focused_fix(),
        Some(&ServiceFix::InstallPlugin),
        "f wraps to the first fix line above the paged view"
    );
    assert!(app.modals.is_empty(), "f opens nothing");
    assert!(
        last.join("\n").contains('┃'),
        "overflow thumb must reach bottom"
    );
    assert_eq!(
        app.services.detail_scroll,
        app.services.detail_max_scroll.get()
    );
    for _ in 0..5 {
        handle_key(&mut app, crate::testutil::key(KeyCode::PageUp));
        let _ = draw_frame(&app, 80, 14);
    }
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    let after = draw_frame(&app, 80, 14).join("\n");
    assert!(
        after.contains("❯ f  wire mcp server"),
        "arrow returns to the selectable fix: {after}"
    );
}

#[test]
fn herdr_diagnostics_and_options_remain_reachable_after_page_scroll() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let warnings: Vec<String> = (0..28)
        .map(|i| format!("registry warning {i:02}"))
        .collect();
    let mut registry = entry(true, Some("0.8.0"), vec![]);
    registry.warnings = warnings;
    let probe = probe(Some("0.8.0"), Some(registry), None);
    let mut app = herdr_options_app(config(true, None, SidebarState::Templated));
    app.tab = crate::tui::app::Tab::Services;
    app.services.herdr = Some(Some(probe.clone()));
    app.services.checks = vec![herdr_check(&probe, app.services.herdr_config.as_ref())];
    let first = draw_frame(&app, 80, 14).join("\n");
    assert!(!first.contains("registry warning 27"));
    let initial_offset = app.services.displayed_scroll.get();
    assert!(
        initial_offset > 0,
        "options must start below the warning body"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::PageUp));
    assert_eq!(
        app.services.detail_scroll,
        initial_offset.saturating_sub(crate::tui::app::page_step(
            app.services.detail_viewport.get() as usize
        ) as u16),
        "PageUp starts at the displayed options, not at an unused scroll field"
    );
    let next = draw_frame(&app, 80, 14).join("\n");
    assert!(
        next.contains("registry warning 27"),
        "one viewport reveals the last warning: {next}"
    );
    assert!(
        app.services.focused_fix().is_none(),
        "hidden herdr fix has no action target"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        app.modals.is_empty(),
        "hidden herdr fix must not open a confirmation"
    );
    let settings = app.config().state.herdr.clone();
    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert_eq!(
        app.config().state.herdr,
        settings,
        "a hidden option cannot be toggled"
    );
    for _ in 0..6 {
        handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
        let _ = draw_frame(&app, 80, 14);
    }
    let end = draw_frame(&app, 80, 14).join("\n");
    assert!(
        end.contains("delegate row text"),
        "last action remains reachable: {end}"
    );
    assert_eq!(
        app.services.detail_scroll,
        app.services.detail_max_scroll.get()
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    let focused = draw_frame(&app, 80, 14).join("\n");
    assert!(
        focused.contains("❯"),
        "arrow restores option focus: {focused}"
    );
}

/// A fix landing under a focused cursor can shrink the problem set; the render
/// must read the cursor through `.get` and never panic on a stale cursor.
#[test]
fn the_plugin_detail_renders_a_shrunk_problem_set_without_panicking() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(plugin_check_with_problems());
    app.services.focus = crate::tui::app::ServicesFocus::Detail;
    app.services.problem_cursor = 1;
    // Shrink the problem set under the focused cursor (as a landed install fix
    // would): one problem remains, the cursor now points past it.
    let mut check = plugin_check_with_problems();
    check.problems.truncate(1);
    app.services.checks = vec![check];
    let (rows, _) = render(&app);
    assert!(
        rows.iter().any(|r| r.contains("install plugin")),
        "the surviving problem still renders:\n{}",
        rows.join("\n")
    );
}

/// The Services detail line renderer truncates to the pane: a path value keeps
/// both ends (middle ellipsis), prose trails — instead of clipping at the
/// border with no marker.
#[test]
fn detail_line_truncates_paths_and_prose_to_the_pane() {
    use ratatui::text::Line;
    let text = |line: &Line| {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    };
    let long_path = "/home/uwuclxdy/.cargo/bin/clauth";
    // value column = width - key_w - 2 = 20 - 4 - 2 = 14 cells; the shared
    // `middle_truncate` keeps 7 head / 6 tail (the head rounds up).
    let line = super::detail_line(&format!("path: {long_path}"), 4, 20);
    assert_eq!(
        text(&line),
        "path  /home/u…clauth",
        "the path keeps both ends around the ellipsis"
    );

    // Prose trails an ellipsis.
    let line = super::detail_line("claude code spawns clauth mcp by name", 6, 20);
    let rendered = text(&line);
    assert!(
        rendered.ends_with('…'),
        "prose trails an ellipsis: {rendered}"
    );
}

/// An outdated plugin version is the reason its row warns, so its value takes
/// the warning tone the install and mcp entry problems take (`installed  no`,
/// `mcp entry  not registered`); a current version stays body text. The line
/// carries the same mark the production version line is built from; that copy
/// itself is pinned by equality in `tests/inline/tui_app.rs`.
#[test]
fn an_outdated_plugin_version_reads_in_the_warning_tone() {
    let value_styles = |text: &str| {
        super::detail_line(text, 7, 80)
            .spans
            .iter()
            .skip(1)
            .map(|s| s.style)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        value_styles(&format!(
            "version: 0.9.0 ({} {})",
            super::OUTDATED_PLUGIN_MARK,
            env!("CARGO_PKG_VERSION")
        )),
        vec![super::theme::warning()],
        "the outdated version warns"
    );
    assert_eq!(
        value_styles(&format!("version: {}", env!("CARGO_PKG_VERSION"))),
        vec![super::theme::body()],
        "a current version stays body text"
    );
}

/// The shunt row's `config` and `binary` path keys truncate on the same shared
/// middle-ellipsis helper, by equality.
#[test]
fn the_shunt_path_keys_truncate_on_the_shared_middle_ellipsis() {
    use ratatui::text::Line;
    let text = |line: &Line| {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    };
    // key_w = 9 (`last exit` is the widest shunt key): value column = 20 - 9 - 2 = 9.
    let line = super::detail_line("config: /home/u/.clauth/gateway.toml", 9, 20);
    assert_eq!(
        text(&line),
        "config     /hom…toml",
        "the config path keeps both ends"
    );
    let line = super::detail_line("binary: /usr/local/bin/shunt", 9, 20);
    assert_eq!(
        text(&line),
        "binary     /usr…hunt",
        "the binary path keeps both ends"
    );
}

/// The Services detail value colouring follows the mcp wording: a server that
/// won't start is danger, an unregistered entry warns, an absent install stays
/// warning, and the healthy words (`registered`, `ok`) read success.
#[test]
fn the_plugin_detail_tones_the_mcp_and_install_values() {
    use ratatui::style::Color;
    let _home = crate::testutil::HomeSandbox::new();
    // The widest key in each fixture is `mcp server` (10), so every value
    // starts 12 cells after its key column.
    let value_fg = |detail: [&str; 3], key: &str| -> Color {
        let app = app_with(Check {
            label: "plugin",
            health: Health::Warn,
            detail: detail.iter().map(|s| s.to_string()).collect(),
            fix: None,
            problems: Vec::new(),
            shunt_focus: Vec::new(),
            shunt_action_start: None,
        });
        let (rows, buf) = render(&app);
        let row_idx = rows
            .iter()
            .position(|r| r.contains(key))
            .unwrap_or_else(|| panic!("no `{key}` row"));
        let row = &rows[row_idx];
        let key_col = row[..row.find(key).expect("key renders")].chars().count();
        buf.content[row_idx * W as usize + key_col + 12].fg
    };

    let unhealthy = [
        "installed: no (marketplace unknown)",
        "mcp entry: not registered",
        "mcp server: won't start (refused)",
    ];
    assert_eq!(
        value_fg(unhealthy, "mcp entry"),
        super::theme::warning_color(),
        "an unregistered entry warns"
    );
    assert_eq!(
        value_fg(unhealthy, "mcp server"),
        super::theme::danger_color(),
        "a server that won't start is danger"
    );
    assert_eq!(
        value_fg(unhealthy, "installed"),
        super::theme::warning_color(),
        "an absent install stays warning"
    );

    let healthy = [
        "installed: yes (user)",
        "mcp entry: registered",
        "mcp server: ok",
    ];
    for key in ["installed", "mcp entry", "mcp server"] {
        assert_eq!(
            value_fg(healthy, key),
            super::theme::success_color(),
            "a healthy `{key}` reads success"
        );
    }
}

/// An unadopted gateway's state reads dim, and a found config renders as a
/// path value beside it.
#[test]
fn an_unadopted_shunt_state_reads_dim() {
    let _home = crate::testutil::HomeSandbox::new();
    let readout = crate::tui::app::StandaloneShunt {
        found: Some(std::path::PathBuf::from("/cfg/shunt.toml")),
        unread_bind: None,
        unread_config: None,
        unread_env: false,
        answer: None,
    };
    let app = app_with(crate::tui::app::shunt_check(
        &shunt_slot(crate::daemon::gateway::GatewayState::Absent),
        false,
        Some(&readout),
    ));
    let (rows, buf) = render(&app);
    let screen = rows.join("\n");
    let row_idx = rows
        .iter()
        .position(|r| r.contains("not adopted"))
        .unwrap_or_else(|| panic!("no state row:\n{screen}"));
    let row = &rows[row_idx];
    let col = row[..row.find("not adopted").unwrap()].chars().count();
    assert_eq!(
        buf.content[row_idx * W as usize + col].fg,
        super::theme::text_dim_color(),
        "`not adopted` is dim:\n{screen}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("found") && r.contains("/cfg/shunt.toml")),
        "the found config renders:\n{screen}"
    );

    // A path wider than the pane keeps both ends: the tree and the file.
    let long = crate::tui::app::StandaloneShunt {
        found: Some(std::path::PathBuf::from(format!(
            "/home/u/{}/shunt.toml",
            "deep/".repeat(40)
        ))),
        unread_bind: None,
        unread_config: None,
        unread_env: false,
        answer: None,
    };
    let app = app_with(crate::tui::app::shunt_check(
        &shunt_slot(crate::daemon::gateway::GatewayState::Absent),
        false,
        Some(&long),
    ));
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    let found = rows
        .iter()
        .find(|r| r.contains("found"))
        .unwrap_or_else(|| panic!("no found row:\n{screen}"));
    assert!(
        found.contains("found  /home/u/deep/")
            && found.contains("…")
            && found.contains("/shunt.toml"),
        "a long found path is middle-truncated:\n{screen}"
    );
}

/// The Services fix grammar (cloudy 2026-10-08/12): from the list, `f` on a row
/// with fix lines descends and lands on its FIRST fix line, opening nothing;
/// repeated `f` walks the row's fix lines and wraps; `↵` on the focused fix
/// line opens its confirm. `f` never opens a modal.
#[test]
fn the_list_f_descends_to_the_first_fix_and_f_walks_the_rows_fixes() {
    use crate::tui::app::{ConfirmAction, Modal, ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(plugin_check_with_problems());
    app.services.problem_cursor = 1;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(app.services.focus, ServicesFocus::Detail, "f descends");
    assert_eq!(
        app.services.focused_fix(),
        Some(&ServiceFix::InstallPlugin),
        "f lands on the row's first fix line, whatever the cursor held"
    );
    assert!(app.modals.is_empty(), "f opens nothing");

    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(
        app.services.focused_fix(),
        Some(&ServiceFix::WireMcpServers),
        "a second f walks to the next fix line"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(
        app.services.focused_fix(),
        Some(&ServiceFix::InstallPlugin),
        "f wraps from the last fix line to the first"
    );
    assert!(app.modals.is_empty(), "no f press opened a modal");

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => assert!(
            matches!(state.on_confirm, ConfirmAction::InstallPlugin),
            "↵ on the focused install line opens the install confirm"
        ),
        other => panic!("expected an install confirm, got {other:?}"),
    }
}

/// `f` on a row with no fix line does nothing: no descend, no modal.
#[test]
fn f_on_a_row_without_a_fix_line_is_inert() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(herdr_check(&healthy_probe(), Some(&healthy_config())));
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(app.services.focus, ServicesFocus::List, "no descend");
    assert!(app.modals.is_empty(), "no modal");
    assert_eq!(app.services.focused_fix(), None);
}

/// The herdr detail's fix line is a walk stop: `f` lands on it from the list
/// and from any option row, `↑↓` reach it from the options, and `↵` on it
/// opens the heal confirm while `↵` on an option still activates the option.
#[test]
fn the_herdr_fix_line_is_a_walk_stop_reached_by_f_and_the_arrows() {
    use crate::tui::app::{ConfirmAction, HERDR_OPTIONS, Modal, ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let heal = |app: &App| {
        matches!(
            app.services.focused_fix(),
            Some(ServiceFix::HealHerdrConfig(_))
        )
    };

    let mut app = herdr_options_app(config(true, None, SidebarState::Templated));
    app.tab = crate::tui::app::Tab::Services;
    app.services.focus = ServicesFocus::List;
    app.services.herdr_options_cursor = 3;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(app.services.focus, ServicesFocus::Detail, "f descends");
    assert!(heal(&app), "f lands on the heal line");
    assert!(app.modals.is_empty(), "f opens nothing");
    let (rows, _) = render(&app);
    assert!(
        rows.iter()
            .any(|r| r.contains('❯') && r.contains("f  heal herdr config")),
        "the focused heal line takes the caret:\n{}",
        rows.join("\n")
    );
    assert!(
        !rows
            .iter()
            .any(|r| r.contains('❯') && r.contains(HERDR_OPTIONS[3].label())),
        "no option row holds the caret while the fix line does:\n{}",
        rows.join("\n")
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(!heal(&app), "↓ leaves the fix line");
    assert_eq!(
        app.services.herdr_options_cursor, 0,
        "↓ lands on the first option"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert!(
        heal(&app),
        "↑ from the first option returns to the fix line"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(
        app.services.herdr_options_cursor,
        HERDR_OPTIONS.len() - 1,
        "↑ from the fix line wraps to the last option"
    );
    assert!(!heal(&app));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(heal(&app), "↓ from the last option wraps to the fix line");
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert!(heal(&app), "f on the only fix line stays there");
    assert!(app.modals.is_empty(), "no f press opened a modal");

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => assert!(
            matches!(state.on_confirm, ConfirmAction::HealHerdrConfig(_)),
            "↵ on the heal line opens the heal confirm"
        ),
        other => panic!("expected a heal confirm, got {other:?}"),
    }
}

/// A healthy herdr detail has no fix line, so the options walk keeps its six
/// stops and `f` does nothing there.
#[test]
fn a_herdr_detail_without_a_fix_keeps_the_six_option_walk() {
    use crate::tui::app::{HERDR_OPTIONS, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(healthy_config());
    app.tab = crate::tui::app::Tab::Services;
    app.services.herdr_options_cursor = HERDR_OPTIONS.len() - 1;
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        app.services.herdr_options_cursor, 0,
        "↓ wraps to the first option"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(
        app.services.herdr_options_cursor,
        HERDR_OPTIONS.len() - 1,
        "↑ wraps to the last option"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(app.services.focused_fix(), None);
    assert!(app.modals.is_empty());
}

/// The shunt card's `f` walks its fix lines generically, in card order,
/// scrolling a fix line below the fold into view, and wraps; config and
/// toggle stops are skipped. `↵` on a fix line opens its confirm.
#[test]
fn the_shunt_f_walks_fix_lines_into_view_and_wraps() {
    use crate::tui::app::{
        Check, ConfirmAction, Health, Modal, ServicesFocus, ShuntFocus, ShuntFocusLine, handle_key,
    };
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut detail: Vec<String> = (0..30).map(|i| format!("plan row {i:02}")).collect();
    detail[5] = "f  move stores in".to_string();
    detail[25] = "f  add admin key".to_string();
    let check = Check {
        label: "shunt",
        health: Health::Warn,
        detail,
        fix: Some(ServiceFix::MoveStoresIn),
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 5,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Enabled { line: 9 },
            ShuntFocus::Fix {
                line: 25,
                fix: ServiceFix::AddAdminKey,
            },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    // Publish the card's geometry with the list focused, as the live loop does.
    let _ = draw_frame(&app, 80, 14);
    app.services.set_shunt_focus(Some(ShuntFocusLine::Enabled));

    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(app.services.focus, ServicesFocus::Detail, "f descends");
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Move),
        "f lands on the card's first fix line, not the held toggle"
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let rows = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Admin),
        "f skips the toggle and scrolls the fold-hidden fix line into view"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains('❯') && r.contains("f  add admin key")),
        "the scrolled-in fix line holds the caret:\n{}",
        rows.join("\n")
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Move),
        "f wraps back to the first fix line, scrolling it into view"
    );
    assert!(app.modals.is_empty(), "no f press opened a modal");

    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => assert!(
            matches!(state.on_confirm, ConfirmAction::AddAdminKey),
            "↵ on the admin fix line opens the admin key confirm"
        ),
        other => panic!("expected an admin key confirm, got {other:?}"),
    }
}

/// A selector row whose detail offers a fix line ends in a trailing `f`
/// (ACCENT + bold, the key's own style) at the row's last content cell; a row
/// with no fix line, healthy or not, carries none. The health dot is
/// unchanged either way.
#[test]
fn a_selector_row_with_a_fix_line_ends_in_a_trailing_f() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let unbound = herdr_check(
        &healthy_probe(),
        Some(&config(true, None, SidebarState::Templated)),
    );
    let too_old = herdr_check(
        &probe(
            Some("0.7.0"),
            Some(entry(true, Some("0.8.0"), vec![])),
            None,
        ),
        Some(&healthy_config()),
    );
    assert!(
        too_old.problems.is_empty(),
        "fixture: a warn row with no fix"
    );
    let app = app_with_checks(vec![
        plugin_check_with_problems(),
        delegates_check(&[], &[]),
        unbound,
    ]);
    let (rows, buf) = render(&app);
    let screen = rows.join("\n");
    // The selector segment between the pane's two side borders.
    let seg = |label: &str| -> (usize, String) {
        let i = rows
            .iter()
            .position(|r| r.contains(&format!("● {label}")))
            .unwrap_or_else(|| panic!("no `{label}` row:\n{screen}"));
        (i, rows[i].split('│').nth(1).unwrap_or_default().to_string())
    };
    let (plugin_row, plugin) = seg("plugin");
    let (_, delegates) = seg("delegates");
    let (_, herdr) = seg("herdr");
    assert!(
        plugin.ends_with(" f "),
        "the plugin row ends in `f` before the padding cell: {plugin:?}"
    );
    assert!(herdr.ends_with(" f "), "the herdr heal row too: {herdr:?}");
    assert_eq!(
        delegates.trim_end(),
        delegates.trim_end().trim_end_matches(" f"),
        "a row with no fix line has no trailing f: {delegates:?}"
    );
    // The `f` cell: ACCENT + bold, on the row's hover tint (the plugin row is
    // the selected one).
    let f_col = 1 + plugin.chars().count() - 2;
    let cell = &buf.content[plugin_row * W as usize + f_col];
    assert_eq!(cell.symbol(), "f", "the mark cell:\n{screen}");
    assert_eq!(cell.fg, super::theme::accent_color(), "the mark is ACCENT");
    assert!(
        cell.modifier.contains(ratatui::style::Modifier::BOLD),
        "the mark is bold"
    );
    assert_eq!(
        cell.bg,
        super::theme::bg_hover(),
        "the selected row keeps its tint"
    );

    let app = app_with(too_old);
    let (rows, _) = render(&app);
    let row = rows
        .iter()
        .find(|r| r.contains("● herdr"))
        .expect("herdr row")
        .split('│')
        .nth(1)
        .unwrap_or_default()
        .to_string();
    assert!(
        !row.trim_end().ends_with(" f"),
        "a warn row with no fix line has no mark: {row:?}"
    );
}

/// The selected row's `BG_HOVER` band is unbroken from the caret's first cell
/// to the list's last content cell, and the label cells promote to `TEXT +
/// bold`. The block's right padding column, where an overflow scrollbar draws,
/// stays untinted.
#[test]
fn the_selected_services_row_tints_from_the_caret_to_the_content_edge() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = app_with_checks(vec![
        crate::tui::app::shunt_check(
            &shunt_slot(crate::daemon::gateway::GatewayState::Absent),
            false,
            None,
        ),
        delegates_check(&[], &[]),
        plugin_check_with_problems(),
        herdr_check(&healthy_probe(), Some(&healthy_config())),
    ]);
    assert!(
        matches!(app.services.focus, crate::tui::app::ServicesFocus::List),
        "fixture: the list owns focus"
    );
    assert_eq!(app.services.cursor, 0, "fixture: the first row is selected");

    let (rows, buf) = render(&app);
    let screen = rows.join("\n");
    let row_idx = rows
        .iter()
        .position(|r| r.contains("● shunt"))
        .unwrap_or_else(|| panic!("no shunt selector row:\n{screen}"));
    let row = &rows[row_idx];
    let cell = |x: usize, y: usize| &buf.content[y * W as usize + x];

    let caret_x = (0..W as usize)
        .find(|&x| cell(x, row_idx).symbol() == "❯")
        .expect("the selected row draws a caret");
    // The list's content rect, measured off the pane's own right border: the
    // block's 1-cell right padding column (the scrollbar's cell) sits just
    // inside that border, and the content's last cell one cell further left.
    let border_x = (caret_x..W as usize)
        .find(|&x| cell(x, row_idx).symbol() == "│")
        .expect("the selector pane draws a right border");
    let content_right = border_x - 2;
    let hover = super::theme::bg_hover();

    for x in caret_x..=content_right {
        assert_eq!(
            cell(x, row_idx).style().bg,
            Some(hover),
            "the selected row's tint has a hole at column {x} of {}:\n{row}",
            content_right
        );
    }
    assert_ne!(
        cell(content_right + 1, row_idx).style().bg,
        Some(hover),
        "the padding/scrollbar column stays untinted:\n{row}"
    );

    let dot_byte = row.find("● shunt").expect("the label renders");
    let label_x = row[..dot_byte].chars().count() + 2;
    for x in label_x..label_x + "shunt".chars().count() {
        let c = cell(x, row_idx);
        assert_eq!(
            c.fg,
            super::theme::text_color(),
            "label cell {x} is TEXT:\n{row}"
        );
        assert!(
            c.modifier.contains(ratatui::style::Modifier::BOLD),
            "label cell {x} is bold:\n{row}"
        );
    }
}

/// A page key hid the plugin walk focus: `f` lands on the first fix line at or
/// below the top of what the pane shows, and wraps to the first fix line when
/// every one sits above it.
#[test]
fn f_from_a_paged_plugin_detail_lands_on_the_next_fix_below_the_view_top() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(plugin_check_with_problems());
    app.services.focus = ServicesFocus::Detail;
    app.services.diagnostic_focus = true;
    // The fixture's fix lines sit at detail lines 2 (install) and 7 (wire).
    app.services.displayed_scroll.set(3);
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(
        app.services.focused_fix(),
        Some(&ServiceFix::WireMcpServers),
        "a view whose top sits between the two fix lines lands on the lower one"
    );
    app.services.diagnostic_focus = true;
    app.services.displayed_scroll.set(7);
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(
        app.services.focused_fix(),
        Some(&ServiceFix::WireMcpServers),
        "a fix line on the view's top row is the one f lands on"
    );
    app.services.diagnostic_focus = true;
    app.services.displayed_scroll.set(8);
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(
        app.services.focused_fix(),
        Some(&ServiceFix::InstallPlugin),
        "with every fix line above the view, f wraps to the first"
    );
}

/// The shunt walk with nothing focused (every stop scrolled out of view) lands
/// on the first fix line at or below the top of the view, not the card's first.
#[test]
fn the_shunt_f_with_no_focus_lands_on_the_first_fix_below_the_view_top() {
    use crate::tui::app::{Check, Health, ShuntFocus, ShuntFocusLine, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut detail: Vec<String> = (0..30).map(|i| format!("plan row {i:02}")).collect();
    detail[5] = "f  move stores in".to_string();
    detail[25] = "f  add admin key".to_string();
    let check = Check {
        label: "shunt",
        health: Health::Warn,
        detail,
        fix: Some(ServiceFix::MoveStoresIn),
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 5,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Enabled { line: 9 },
            ShuntFocus::Fix {
                line: 25,
                fix: ServiceFix::AddAdminKey,
            },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    // Rows 12..20 show no stop: clear the focus and scroll the view there, so
    // nothing is focused and no stop is in the padded band (the render's
    // re-settle leaves the focus none).
    app.services.shunt_scroll.set(12);
    app.services.set_shunt_focus(None);
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.shunt_focus_kind(),
        None,
        "fixture: no stop in view"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Admin),
        "f lands on the fix line below the view top, not the one above it"
    );
}

/// With nothing focused, `f` takes the next FIX below the view, skipping a
/// toggle stop that sits nearer: the no-focus pick keeps `f`'s fix filter.
#[test]
fn the_shunt_f_with_no_focus_skips_a_toggle_for_the_next_fix() {
    use crate::tui::app::{Check, Health, ShuntFocus, ShuntFocusLine, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut detail: Vec<String> = (0..30).map(|i| format!("plan row {i:02}")).collect();
    detail[5] = "f  move stores in".to_string();
    detail[25] = "f  add admin key".to_string();
    let check = Check {
        label: "shunt",
        health: Health::Warn,
        detail,
        fix: Some(ServiceFix::MoveStoresIn),
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 5,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Enabled { line: 22 },
            ShuntFocus::Fix {
                line: 25,
                fix: ServiceFix::AddAdminKey,
            },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    // Rows 12..20 hold no stop; the toggle (22) and the admin fix (25) both
    // sit below the view, the toggle nearer.
    app.services.shunt_scroll.set(12);
    app.services.set_shunt_focus(None);
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.shunt_focus_kind(),
        None,
        "fixture: no stop in view"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Admin),
        "f skips the nearer toggle for the next fix"
    );
}

/// With nothing focused and every fix above the view's padded band (the last
/// one just dropped by a text scroll), `f` wraps to the card's FIRST fix, never
/// back onto the fix the scroll dropped.
#[test]
fn the_shunt_f_with_no_focus_wraps_to_the_first_fix_not_the_dropped_one() {
    use crate::tui::app::{Check, Health, ShuntFocus, ShuntFocusLine, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut detail: Vec<String> = (0..30).map(|i| format!("plan row {i:02}")).collect();
    detail[2] = "f  move stores in".to_string();
    detail[5] = "f  add admin key".to_string();
    let check = Check {
        label: "shunt",
        health: Health::Warn,
        detail,
        fix: Some(ServiceFix::MoveStoresIn),
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 2,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Fix {
                line: 5,
                fix: ServiceFix::AddAdminKey,
            },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Admin),
        "fixture: ↓ takes the second fix"
    );
    // No stop lies below the admin fix: each ↓ scrolls card text until the
    // admin row nears the top and the caret drops.
    let mut dropped = false;
    for _ in 0..20 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let _ = draw_frame(&app, 80, 14);
        if app.services.shunt_focus_kind().is_none() {
            dropped = true;
            break;
        }
    }
    assert!(dropped, "fixture: a ↓ text scroll drops the caret");
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Move),
        "f wraps to the first fix, not the one the scroll dropped"
    );
}

/// A heal that lands while the heal line holds focus leaves the option walk
/// live: the next arrow steps from the option cursor, never from the gone line.
#[test]
fn a_heal_that_clears_the_fix_line_hands_focus_back_to_the_options() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(config(true, None, SidebarState::Templated));
    app.tab = crate::tui::app::Tab::Services;
    app.services.herdr_options_cursor = 2;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert!(
        app.services.focused_fix().is_some(),
        "fixture: on the heal line"
    );
    // The recompute after a landed heal: the check offers no fix line any more.
    let healed = healthy_config();
    app.services.herdr_config = Some(healed.clone());
    app.services.checks = vec![herdr_check(&healthy_probe(), Some(&healed))];
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        app.services.herdr_options_cursor, 3,
        "↓ steps from the option cursor once the heal line is gone"
    );
    assert_eq!(app.services.focused_fix(), None);
}

/// `↵` on the herdr row lands on the option cursor, never on a heal line `f`
/// left focused before an ascend.
#[test]
fn enter_descending_into_herdr_lands_on_the_options_not_a_held_heal_line() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(config(true, None, SidebarState::Templated));
    app.tab = crate::tui::app::Tab::Services;
    app.services.focus = ServicesFocus::List;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert!(
        app.services.focused_fix().is_some(),
        "fixture: on the heal line"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert_eq!(
        app.services.focus,
        ServicesFocus::List,
        "fixture: esc ascends"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    assert_eq!(
        app.services.focused_fix(),
        None,
        "↵ descends onto the option cursor"
    );
}

/// The trailing `f` drops whole where the 3-cell gap cannot fit: `  ● delegates`
/// is 13 cells, so the mark needs 13 + 3 + 1 = 17.
#[test]
fn the_trailing_f_drops_whole_when_its_gap_does_not_fit() {
    let text = |w: usize| -> String {
        super::selector_row(Health::Warn, "delegates", true, false, false, w)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    };
    assert_eq!(text(17), "  ● delegates   f");
    assert_eq!(text(16), "  ● delegates   ");
}

/// The footer names what each key does under the fix grammar: on a focused
/// fix line `↵ <verb>`; `f next fix` only while the press would move focus
/// (another fix line, or focus off every fix line).
#[test]
fn the_footer_names_enter_for_the_focused_fix_and_f_for_the_next() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let hints = |app: &App| super::super::footer::services_hints(app);

    let mut app = app_with(plugin_check_with_problems());
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "row"),
            ("↵", "detail"),
            ("r", "refresh"),
            ("f", "next fix"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "the list advertises f as the walk, never as a fix"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "problem"),
            ("r", "refresh"),
            ("↵", "install plugin"),
            ("f", "next fix"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "a focused fix line names ↵ with its verb, and f the next fix"
    );

    let mut app = herdr_options_app(config(true, None, SidebarState::Templated));
    app.tab = crate::tui::app::Tab::Services;
    app.services.focus = ServicesFocus::List;
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "row"),
            ("↵", "heal herdr config"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "on the only fix line f would not move, so it has no hint"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "row"),
            ("space/↵", "cycle / toggle"),
            ("r", "refresh"),
            ("f", "next fix"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "off the fix line, f walks back to it"
    );
}

// ── delegates detail ────────────────────────────────────────────────────────────

fn app_with_delegates(delegates: Vec<jobs::StoredJob>) -> App {
    let mut app = app_with_checks(vec![delegates_check(&delegates, &[])]);
    app.services.delegates = delegates;
    app
}

/// A realistic wall clock rather than a round synthetic one: every row's state is
/// chosen by comparing its own stamps against this, and a year-2096 `now` routes
/// whole classes into one branch.
///
/// Only the PURE tests may pin it. A `TestBackend` render reaches the detail
/// through `draw`, which reads the real clock, so every render fixture below is
/// seeded relative to `now_ms()` and asserts what the clock cannot move — the
/// state words, the accounts, which fields are present, and the steer line. The
/// exact figures are pinned where `now` is an argument.
const NOW: u64 = 1_800_000_000_000;

/// The streaming shape a real reserve writes: no wall clock, the default idle
/// guard.
fn running_spec(job_id: &str, profile: &str, started_at: u64, kind: RecordKind) -> RunningSpec {
    RunningSpec {
        job_id: job_id.to_string(),
        profile: profile.to_string(),
        started_at,
        recorded_at: started_at,
        timeout_secs: 0,
        endpoint: None,
        provider: None,
        isolated: false,
        cwd: None,
        spawned_by: None,
        host_session: None,
        host_pid: 0,
        idle_secs: Some(300),
        kind,
        owner_pid: 0,
        owner_started_at: 0,
    }
}

/// Seed one row of each state through the store's OWN writers, then list it back
/// — so the fixture is bytes the producer really emits rather than a struct
/// literal that agrees with whatever the fields are today.
fn seed_every_state(now: u64) -> Vec<jobs::StoredJob> {
    // Freshest first once listed: each anchor is further back than the last.
    // Only some records carry an origin: the others are the shape an older
    // server wrote.
    jobs::write_heartbeat(
        &RunningSpec {
            cwd: Some("/home/u/repos/app".to_string()),
            spawned_by: Some("cld".to_string()),
            ..running_spec("d-bg-0", "uwuclxdy", now - 134_000, RecordKind::Collectable)
        },
        now - 12_000,
        "reading the plan doc",
    )
    .unwrap();
    jobs::write_heartbeat(
        &running_spec("d-blk-0", "kerry", now - 40_000, RecordKind::Liveness),
        now - 30_000,
        "still thinking",
    )
    .unwrap();
    // Written as bytes rather than through `write_done`, which stamps the real
    // clock: when a job finished is exactly the field this row is dated by, so
    // the test has to choose it.
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-old-0.json"),
        serde_json::to_vec(&serde_json::json!({
            "job_id": "d-old-0",
            "profile": "DS8",
            "state": "done",
            "started_at": now - 1_800_000,
            "done_at": now - 900_000,
            "envelope": { "result": "finished a while back" },
            "cwd": "/home/u/repos/api",
            "spawned_by": "uwuclxdy",
        }))
        .unwrap(),
    )
    .unwrap();
    // Finished on this very millisecond: the one input that reaches
    // `age_phrase`'s zero branch, without which the row reads `now ago`.
    std::fs::write(
        dir.join("d-fresh-0.json"),
        serde_json::to_vec(&serde_json::json!({
            "job_id": "d-fresh-0",
            "profile": "glm2",
            "state": "done",
            "started_at": now - 5_000,
            "done_at": now,
            "envelope": { "result": "just finished" },
        }))
        .unwrap(),
    )
    .unwrap();
    // Silent far past the corpse window: its server is gone.
    jobs::write_running(&running_spec(
        "d-dead-0",
        "glm1",
        now - 90_000_000,
        RecordKind::Collectable,
    ))
    .unwrap();
    jobs::list_banded(now)
}

/// The delegates detail keeps its two-unit sub-minute age: every liveness age
/// is read against a 300 s idle guard, so 5 s vs 55 s is the whole signal and
/// collapsing it to `just now` would lose it. Deliberately NOT `relative_age`.
#[test]
fn delegate_age_keeps_the_sub_minute_two_unit_signal() {
    assert_eq!(super::age_phrase(5), "5s ago");
    assert_eq!(super::age_phrase(55), "55s ago");
    assert_eq!(super::age_phrase(65), "1m 5s ago");
}

/// The exact rows, at a `now` the test owns: elapsed, the spawning account and
/// the deadline that lands first, then the run's directory, each from the
/// fields the record carries; a record without an origin shows neither.
#[test]
fn a_delegate_row_carries_the_figures_its_own_record_holds() {
    use ratatui::text::Line;
    let _home = crate::testutil::HomeSandbox::new();
    let cells = super::delegate_cells(&seed_every_state(NOW), NOW);
    let facts = |account: &str| -> String {
        cells
            .iter()
            .find(|c| c.profile == account)
            .unwrap_or_else(|| panic!("no row for `{account}`"))
            .facts
            .join(" · ")
    };
    assert_eq!(
        facts("uwuclxdy"),
        "elapsed 2m 14s · spawned by cld · idle-kill in 4m 48s",
        "a running row counts from its own stamps and names its spawner",
    );
    assert_eq!(
        facts("kerry"),
        "elapsed 40s · idle-kill in 4m 30s",
        "a record with no origin names no spawner",
    );
    assert_eq!(
        facts("glm2"),
        "finished just now",
        "a job that finished this millisecond reads `just now`, never `now ago`",
    );
    assert_eq!(
        facts("DS8"),
        "finished 15m ago · spawned by uwuclxdy",
        "a finished job is dated by its finish and keeps its spawner",
    );

    let line_text = |line: &Line| {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    };
    let row = |account: &str| {
        let cell = cells.iter().find(|c| c.profile == account).unwrap();
        line_text(&super::delegate_row_line(cell, 8, 100, false, false))
    };
    assert_eq!(
        row("uwuclxdy"),
        "  ● running   uwuclxdy  elapsed 2m 14s · spawned by cld · idle-kill in 4m 48s  /home/u/repos/app",
        "the run's directory closes the row, never its last words",
    );
    assert_eq!(
        row("kerry"),
        "  ● blocking  kerry     elapsed 40s · idle-kill in 4m 30s",
        "a record with no origin ends on its facts",
    );
    assert_eq!(
        row("DS8"),
        "  ● done      DS8       finished 15m ago · spawned by uwuclxdy  /home/u/repos/api",
    );
    assert_eq!(
        facts("glm1"),
        "last seen 1d 1h ago",
        "a corpse by when it was last heard from",
    );
}

/// The three hues the four states map onto, off the styled buffer.
///
/// The detail had NO colour assertion at all until the `JobPhase` fold, and the
/// arm that mattered was `blocking`: it is LIVE but not COLLECTABLE, so a fold
/// that reconstructed the hue from `is_collectable()` instead of the live band
/// would have recoloured it to `done`'s success green with every test in the
/// repo still green. The rendered rows are plain strings; only the buffer holds
/// the style.
///
/// Mapped to the theme here rather than through `state_color`, so a regression
/// in that mapping reds this instead of moving both sides together — the same
/// rule `assert_dot` plays by for the health dot.
#[test]
fn each_delegate_state_carries_its_own_hue() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_delegates(seed_every_state(crate::usage::now_ms()));
    let (rows, buf) = render(&app);
    let screen = rows.join("\n");

    let hue = |account: &str| {
        let row_idx = rows
            .iter()
            .position(|r| r.contains(account))
            .unwrap_or_else(|| panic!("no row for `{account}`:\n{screen}"));
        let row = &rows[row_idx];
        // The dot nearest the account: the selector pane's own `●` can share
        // the first row.
        let byte = row[..row.find(account).unwrap()]
            .rfind(['●', '○'])
            .expect("state dot renders");
        let col = row[..byte].chars().count();
        buf.content[row_idx * W as usize + col].fg
    };

    assert_eq!(
        hue("uwuclxdy"),
        super::theme::accent_color(),
        "a running delegate is accent:\n{screen}"
    );
    assert_eq!(
        hue("kerry"),
        super::theme::accent_color(),
        "and so is a blocking one — it bands with running, not with done:\n{screen}"
    );
    assert_eq!(
        hue("DS8"),
        super::theme::success_color(),
        "a finished job is success:\n{screen}"
    );
    assert_eq!(
        hue("glm1"),
        super::theme::text_dim_color(),
        "an orphan is dim; the word carries the charge:\n{screen}"
    );
}

#[test]
fn the_delegates_detail_names_each_state_and_carries_the_steer_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_delegates(seed_every_state(crate::usage::now_ms()));
    let (rows, _) = render(&app);
    let screen = rows.join("\n");

    // Each row is identified by its own ACCOUNT, so a needle can never be read
    // off the wrong line.
    let row_for = |account: &str| -> String {
        rows.iter()
            .find(|r| r.contains(account))
            .unwrap_or_else(|| panic!("no row for `{account}`:\n{screen}"))
            .clone()
    };
    assert!(
        row_for("uwuclxdy").contains("● running"),
        "a background job reads as running:\n{screen}"
    );
    assert!(
        row_for("kerry").contains("● blocking"),
        "a run whose caller still holds the line reads apart from it:\n{screen}"
    );
    assert!(
        row_for("DS8").contains("● done"),
        "a finished job reads as done:\n{screen}"
    );
    assert!(
        row_for("glm1").contains("○ orphaned"),
        "and a corpse is drawn as one, never as live:\n{screen}"
    );

    let running = row_for("uwuclxdy");
    // The detail pane is narrower than the old full-width third panel, so only
    // the leading figures are guaranteed room; the exact deadline figures are
    // pinned by `a_delegate_row_carries_the_figures_its_own_record_holds`.
    for needle in ["elapsed ", "spawned by cld"] {
        assert!(
            running.contains(needle),
            "`{needle}` missing from the running row:\n{running}"
        );
    }
    assert!(
        row_for("DS8").contains("finished "),
        "a done row is dated by its finish:\n{screen}"
    );
    assert!(
        screen.contains("manage delegates in clauth app on web or mobile (coming soon)"),
        "the steer line renders under the list:\n{screen}"
    );
}

/// What the detail draws and what `monitor` tells the model about ONE record
/// must not be able to disagree.
///
/// Every figure asserted here is read OUT of `monitor`'s payload and then looked
/// for in the rendered row, so a detail that grew a second copy of the arithmetic
/// reds this even when its own numbers look plausible.
#[test]
fn the_delegates_detail_reports_what_monitor_reports_for_the_same_record() {
    let _home = crate::testutil::HomeSandbox::new();
    // A pinned-`--output-format` run, so BOTH deadlines are present and the
    // detail has to pick the one that lands first — and one handed off
    // mid-flight, so `recorded_at` sits well after `started_at`. That gap is
    // what makes the test discriminate: on a record where the two are equal
    // (every job that started out background), a row counting elapsed from the
    // wrong field agrees with `monitor` by accident.
    let spec = RunningSpec {
        timeout_secs: 900,
        idle_secs: Some(300),
        recorded_at: NOW - 120_000,
        ..running_spec(
            "d-pin-0",
            "uwuclxdy",
            NOW - 200_000,
            RecordKind::Collectable,
        )
    };
    jobs::write_heartbeat(&spec, NOW - 47_000, "mid-run").unwrap();

    let stored = jobs::list_banded(NOW);
    let record: &JobRecord = &stored[0].record;
    assert_eq!(record.state, JobState::Running, "fixture control");

    let payload = crate::mcp::running_payload_for_test(&record.job_id, record, NOW);
    let secs = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| panic!("monitor reports {key}: {payload}"))
    };
    let cells = super::delegate_cells(&stored, NOW);
    let facts = cells[0].facts.join(" · ");

    assert!(
        facts.contains(&format!(
            "elapsed {}",
            crate::usage::humanize_duration(secs("elapsed_secs") as i64)
        )),
        "elapsed disagrees with monitor's {payload}: {facts}"
    );
    // The idle guard lands first on this fixture, so that is the countdown the
    // row spends its one cell on — and it is monitor's own figure.
    let idle = secs("idle_kill_in_secs");
    assert!(
        idle < secs("wall_kill_in_secs"),
        "fixture control: the idle guard is the deadline that fires: {payload}"
    );
    assert!(
        facts.contains(&format!(
            "idle-kill in {}",
            crate::usage::humanize_duration(idle as i64)
        )),
        "the next deadline disagrees with monitor's {payload}: {facts}"
    );
}

/// More delegates than the detail holds: every job is a row and the list
/// scrolls, so no `+N more` row ever renders — blurred or descended.
#[test]
fn the_delegates_detail_never_caps_the_list_with_a_more_row() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    for i in 0..22 {
        jobs::write_heartbeat(
            &running_spec(
                &format!("d-many-{i}"),
                &format!("acct{i}"),
                now - 10_000 - i as u64,
                RecordKind::Collectable,
            ),
            now - 1_000 - i as u64,
            "working",
        )
        .unwrap();
    }
    let app = app_with_delegates(jobs::list_banded(now));

    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    assert!(
        screen.contains("acct0"),
        "the newest delegate is the first row:\n{screen}"
    );
    assert!(
        !screen.contains("more"),
        "no `+N more` row when blurred:\n{screen}"
    );

    let mut descended = app;
    handle_key(&mut descended, crate::testutil::key(KeyCode::Enter));
    let (rows, _) = render(&descended);
    let screen = rows.join("\n");
    assert!(
        !screen.contains("more"),
        "no `+N more` row when descended:\n{screen}"
    );
}

/// A delegate's `cwd` is the calling model's own argument, so a terminal
/// escape or a bidi override in it reaches the row only as its visible escape,
/// the way the shunt row shows an untrusted slot string.
#[test]
fn a_delegate_cwd_renders_control_and_bidi_characters_escaped() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    jobs::write_heartbeat(
        &RunningSpec {
            cwd: Some("/w/\u{1b}[2J/\u{202e}x".to_string()),
            ..running_spec("d-esc-0", "acct", now - 5_000, RecordKind::Collectable)
        },
        now - 1_000,
        "",
    )
    .unwrap();
    let cells = super::delegate_cells(&jobs::list_banded(now), now);
    assert_eq!(
        cells[0].cwd.as_deref(),
        Some("/w/\\u{1b}[2J/\\u{202e}x"),
        "the escape and the override render as text, never as bytes",
    );
}

/// The pure row/expansion contract: the run's directory is what gives way (a
/// middle `…`, then the whole of it) on the one-line row, and the selected
/// row's expansion carries the whole directory, wrapped instead of truncated.
#[test]
fn delegate_line_and_expansion_pin_the_directory_exactly() {
    use crate::mcp::jobs::JobPhase;
    use ratatui::text::Line;
    let line_text = |line: &Line| {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    };

    let _home = crate::testutil::HomeSandbox::new();

    // One running row with a long directory: at full width it rides whole; at
    // a narrow width it gives way in the middle, keeping the tree and the
    // leaf; with less room than `CWD_MIN_W` it is dropped whole.
    let one = super::DelegateCells {
        state: JobPhase::Running,
        profile: "acct".to_string(),
        facts: vec!["elapsed 5s".to_string()],
        cwd: Some("/home/u/repos/rs/clauth".to_string()),
    };
    assert_eq!(
        line_text(&super::delegate_row_line(&one, 4, 100, false, false)),
        "  ● running   acct  elapsed 5s  /home/u/repos/rs/clauth",
        "the directory rides whole when it fits",
    );
    // 30 cells of row (the reserved 2-cell gutter included) and a 2-cell gap
    // before the directory: 45 leaves it 13, 44 leaves it exactly `CWD_MIN_W`,
    // 43 one short of it.
    assert_eq!(
        line_text(&super::delegate_row_line(&one, 4, 45, false, false)),
        "  ● running   acct  elapsed 5s  /home/…clauth",
        "a narrow row keeps both ends of the directory",
    );
    assert_eq!(
        line_text(&super::delegate_row_line(&one, 4, 44, false, false)),
        "  ● running   acct  elapsed 5s  /home/…lauth",
        "the narrowest room a directory still renders in",
    );
    assert_eq!(
        line_text(&super::delegate_row_line(&one, 4, 43, false, false)),
        "  ● running   acct  elapsed 5s",
        "a row with no room for a readable directory drops it",
    );

    // The expansion carries the whole directory, wrapped at the width the row
    // would have truncated it at, each line indented under the row.
    let expanded = super::delegate_expansion_lines(&one, 45);
    let expanded: Vec<String> = expanded.iter().map(line_text).collect();
    assert_eq!(
        expanded,
        vec!["  elapsed 5s", "  /home/u/repos/rs/clauth"],
        "the expansion keeps the fact and the whole directory, no `…`",
    );
    // A directory longer than the row width wraps onto indented continuation
    // lines instead of truncating.
    let long = super::DelegateCells {
        state: JobPhase::Running,
        profile: "acct".to_string(),
        facts: vec!["elapsed 5s".to_string()],
        cwd: Some("/home/u/a/very/long/run/directory/that/overflows/the/pane/width".to_string()),
    };
    let expanded: Vec<String> = super::delegate_expansion_lines(&long, 45)
        .iter()
        .map(line_text)
        .collect();
    assert!(
        expanded
            .iter()
            .all(|l| l.chars().count() <= 45 && l.starts_with("  ")),
        "every expansion line is indented and fits the pane: {expanded:?}"
    );
    let joined: String = expanded
        .iter()
        .map(|l| l.trim_start())
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(
        joined, "elapsed 5s/home/u/a/very/long/run/directory/that/overflows/the/pane/width",
        "the full directory survives, wrapped, never middle-truncated: {expanded:?}"
    );
}

/// The delegates detail's content rendered directly (no panel chrome), so the
/// footnote and scrollbar geometry is readable row-by-row.
fn render_delegates_detail(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw_delegates_detail(f, f.area(), app))
        .unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
}

/// The detail pane's content of a rendered full-frame row (after the `││` seam),
/// trimmed of the pane border and its 1-cell padding.
fn detail_of(row: &str) -> &str {
    row.split("││")
        .nth(1)
        .unwrap_or("")
        .trim_matches('│')
        .trim()
}

/// Seed `count` running jobs, newest first, returning their banded listing.
fn seed_walk_jobs(count: usize) -> Vec<jobs::StoredJob> {
    let now = crate::usage::now_ms();
    for i in 0..count {
        jobs::write_heartbeat(
            &running_spec(
                &format!("d-walk-{i:02}"),
                &format!("acct{i:02}"),
                now - 10_000 - i as u64,
                RecordKind::Collectable,
            ),
            now - 1_000 - i as u64,
            "working",
        )
        .unwrap();
    }
    jobs::list_banded(now)
}

// #B1: `↵` descends; walking ↓ visits every job in order and wraps; PageDown
// reaches the last job and stops; each selected job is drawn on screen.
#[test]
fn walking_the_delegates_detail_visits_every_job_and_wraps() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let count = 22;
    let mut app = app_with_delegates(seed_walk_jobs(count));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(
        app.services.focus,
        ServicesFocus::Detail,
        "↵ descends into the detail"
    );

    for i in 0..count {
        assert_eq!(app.services.delegates_cursor, i, "cursor at job {i}");
        let (rows, _) = render(&app);
        let screen = rows.join("\n");
        // The caret must sit on the job's own row in the DETAIL half — the
        // selector pane's caret shares the physical row and must not satisfy it.
        assert!(
            rows.iter().any(|r| {
                detail_of(r).contains('❯') && detail_of(r).contains(&format!("acct{i:02}"))
            }),
            "job {i} is drawn on screen after each step:\n{screen}"
        );
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
    assert_eq!(
        app.services.delegates_cursor, 0,
        "↓ past the last job wraps to the first"
    );

    // PageDown pages toward the end and stops there, however many presses.
    for _ in 0..count {
        render(&app);
        handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    }
    assert_eq!(
        app.services.delegates_cursor,
        count - 1,
        "PageDown stops at the last job"
    );
    let (rows, _) = render(&app);
    assert!(
        rows.iter()
            .any(|r| r.contains('❯') && r.contains(&format!("acct{:02}", count - 1))),
        "the last job is drawn on screen:\n{}",
        rows.join("\n")
    );
}

// #B2: at 45 columns the selected job's expansion shows its whole run
// directory, pinned by equality on the expansion rows — the row drops the
// directory (too narrow) and the expansion never middle-truncates it.
#[test]
fn the_selected_delegate_expansion_shows_the_whole_directory_at_45_columns() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    let cwd = "/very/long/run/directory/here";
    jobs::write_heartbeat(
        &RunningSpec {
            cwd: Some(cwd.to_string()),
            spawned_by: Some("cld".to_string()),
            idle_secs: None,
            ..running_spec("d-exp-0", "acct", now - 5_000, RecordKind::Collectable)
        },
        now - 1_000,
        "working",
    )
    .unwrap();
    let mut app = app_with_delegates(jobs::list_banded(now));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);

    let rows = render_delegates_detail(&app, 45, 20);
    let trimmed: Vec<&str> = rows.iter().map(|r| r.trim_end()).collect();
    let screen = trimmed.join("\n");
    assert!(
        trimmed.contains(&"  /very/long/run/directory/here"),
        "the whole directory pins by equality on its expansion row:\n{screen}"
    );
    assert!(
        trimmed.contains(&"  spawned by cld"),
        "the spawner pins by equality on its expansion row:\n{screen}"
    );
    assert!(
        !screen.contains('…'),
        "no ellipsis anywhere — the directory is never truncated:\n{screen}"
    );
}

// #B4: the steer line sits on the pane's last interior row with a margin row
// above it, descended and blurred; the scrollbar spans the scroll area only.
#[test]
fn the_delegates_steer_line_pins_with_a_margin_and_a_scoped_scrollbar() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let jobs = seed_walk_jobs(22);
    let blurred = app_with_delegates(jobs.clone());
    let mut descended = app_with_delegates(jobs);
    handle_key(&mut descended, crate::testutil::key(KeyCode::Enter));

    for (label, app) in [("blurred", &blurred), ("descended", &descended)] {
        let (rows, _) = render(app);
        let screen = rows.join("\n");
        let steer = rows
            .iter()
            .position(|r| detail_of(r).starts_with("manage delegates"))
            .unwrap_or_else(|| panic!("[{label}] no steer line:\n{screen}"));
        assert_eq!(
            steer,
            H as usize - 2,
            "[{label}] the steer line pins on the last interior row:\n{screen}"
        );
        assert!(
            detail_of(&rows[steer - 1]).is_empty(),
            "[{label}] a blank margin row sits above the steer line:\n{screen}"
        );
        // The scrollbar's column is the detail pane's right-padding cell (x=W-2).
        let track = |row_idx: usize| rows[row_idx].chars().nth(W as usize - 2).unwrap_or(' ');
        assert!(
            !matches!(track(steer - 1), '┊' | '┃') && !matches!(track(steer), '┊' | '┃'),
            "[{label}] the margin and steer rows carry no scrollbar cell:\n{screen}"
        );
        assert!(
            (0..steer - 1).any(|idx| matches!(track(idx), '┊' | '┃')),
            "[{label}] the scroll area itself overflows and draws the track:\n{screen}"
        );
    }
}

// #B5: `esc` and `←` ascend; with zero jobs `↵` does not descend and the
// footer advertises no `↵` for the row.
#[test]
fn delegates_ascend_and_zero_jobs_do_not_descend() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with_delegates(seed_walk_jobs(2));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert_eq!(app.services.focus, ServicesFocus::List, "esc ascends");
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    handle_key(&mut app, crate::testutil::key(KeyCode::Left));
    assert_eq!(app.services.focus, ServicesFocus::List, "← ascends");

    let mut empty = app_with_delegates(Vec::new());
    handle_key(&mut empty, crate::testutil::key(KeyCode::Enter));
    assert_eq!(
        empty.services.focus,
        ServicesFocus::List,
        "↵ does not descend into a job-less detail"
    );
    let hints = super::super::footer::services_hints(&empty);
    assert!(
        !hints.iter().any(|(k, _)| *k == "↵"),
        "the footer advertises no `↵` for a job-less delegates row: {hints:?}"
    );
}

// #B6: a refresh that adds a newer job above the selected one keeps the same
// job selected — the selection follows the job id, not its index.
#[test]
fn a_refresh_that_inserts_a_newer_job_keeps_the_same_job_selected() {
    use crate::tui::app::ServicesFocus;
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    jobs::write_heartbeat(
        &running_spec("d-b", "b", now - 20_000, RecordKind::Collectable),
        now - 2_000,
        "working",
    )
    .unwrap();
    jobs::write_heartbeat(
        &running_spec("d-a", "a", now - 10_000, RecordKind::Collectable),
        now - 1_000,
        "working",
    )
    .unwrap();
    let mut app = app_with_delegates(jobs::list_banded(now));
    app.services.focus = ServicesFocus::Detail;
    // Select job `d-b` (the older one, index 1).
    app.services.delegates_cursor = 1;
    app.services.delegates_selected_id = Some("d-b".to_string());

    // A refresh lands a newer job above it, driven through the production
    // refresh path (which owns the settle call), not by calling the settle
    // directly.
    jobs::write_heartbeat(
        &running_spec("d-c", "c", now - 5_000, RecordKind::Collectable),
        now - 500,
        "working",
    )
    .unwrap();
    crate::tui::app::recompute_services_checks(
        &mut app,
        false,
        crate::tui::app::ShuntRefresh::Keep,
    );

    assert_eq!(
        app.services.delegates_cursor, 2,
        "the selection follows `d-b` to its new index"
    );
    assert_eq!(
        app.services.delegates_selected_id.as_deref(),
        Some("d-b"),
        "the tracked id is unchanged"
    );
}

// #B7: no key while descended removes, cancels or stops a job — every bound
// key leaves the job store untouched.
#[test]
fn no_key_while_descended_stops_or_removes_a_job() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    for i in 0..5 {
        jobs::write_heartbeat(
            &running_spec(
                &format!("d-stop-{i}"),
                &format!("acct{i}"),
                now - 10_000 - i as u64,
                RecordKind::Collectable,
            ),
            now - 1_000 - i as u64,
            "working",
        )
        .unwrap();
    }
    let snapshot: Vec<String> = jobs::list_banded(now)
        .iter()
        .map(|j| serde_json::to_string(&j.record).expect("record serializes"))
        .collect();

    let keys = [
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Esc,
        KeyCode::Enter,
        KeyCode::Char(' '),
        KeyCode::Char('f'),
        KeyCode::Char('q'),
        KeyCode::Char('a'),
        KeyCode::Char('?'),
        KeyCode::Char('x'),
        KeyCode::Char('c'),
        KeyCode::Char('d'),
        KeyCode::Char('s'),
        KeyCode::Delete,
        KeyCode::Backspace,
    ];
    for code in keys {
        let mut app = app_with_delegates(jobs::list_banded(now));
        handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
        handle_key(&mut app, crate::testutil::key(code));
        let after: Vec<String> = jobs::list_banded(now)
            .iter()
            .map(|j| serde_json::to_string(&j.record).expect("record serializes"))
            .collect();
        assert_eq!(
            after, snapshot,
            "key {code:?} changed the job store while descended"
        );
    }
}

// #F4: the expansion keeps every byte of the escaped directory — a name with a
// run of spaces keeps them, since chunking is by display width, never on
// whitespace.
#[test]
fn the_delegate_expansion_keeps_every_byte_of_the_directory() {
    use crate::mcp::jobs::JobPhase;
    let _home = crate::testutil::HomeSandbox::new();
    let cells = super::DelegateCells {
        state: JobPhase::Running,
        profile: "acct".to_string(),
        facts: vec!["elapsed 5s".to_string()],
        cwd: Some("/home/u/My  Dir/x".to_string()),
    };
    let lines: Vec<String> = super::delegate_expansion_lines(&cells, 45)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert_eq!(
        lines,
        vec!["  elapsed 5s", "  /home/u/My  Dir/x"],
        "the directory's double space survives, never collapsed"
    );
}

// #F5: every delegate row reserves the 2-cell caret gutter, so selecting a row
// does not shift its columns — the state word opens at col 4 on every row.
#[test]
fn every_delegate_row_reserves_the_caret_gutter() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_delegates(seed_walk_jobs(3));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let rows = render_delegates_detail(&app, 70, 20);
    // Three job rows (the expansion lines carry no state word); every one
    // reserves the gutter so `running` opens at col 4.
    let job_rows: Vec<&String> = rows.iter().filter(|r| r.contains("running")).collect();
    assert_eq!(
        job_rows.len(),
        3,
        "three job rows render:\n{}",
        rows.join("\n")
    );
    for (i, row) in job_rows.iter().enumerate() {
        let byte = row.find("running").unwrap();
        assert_eq!(
            row[..byte].chars().count(),
            4,
            "the state word opens at col 4 (gutter + dot) on job row {i}:\n{}",
            rows.join("\n")
        );
    }
}

// #F6: once descended and ascended, the blurred pane keeps the last-selected
// job's tint and scroll offset; the caret and expansion drop, and the list is
// not snapped to the top.
#[test]
fn the_blurred_delegates_detail_keeps_the_last_selected_tint_and_offset() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_delegates(seed_walk_jobs(22));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    for _ in 0..20 {
        render(&app);
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
    assert_eq!(app.services.delegates_cursor, 20);
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert_eq!(app.services.focus, ServicesFocus::List);

    let (rows, buf) = render(&app);
    let screen = rows.join("\n");
    let row_idx = rows
        .iter()
        .position(|r| detail_of(r).contains("acct20"))
        .unwrap_or_else(|| panic!("the held offset keeps job 20 on screen:\n{screen}"));
    let full = &rows[row_idx];
    assert!(
        !detail_of(full).starts_with('❯'),
        "the blurred pane drops the caret:\n{screen}"
    );
    let byte = full.find("acct20").expect("job 20's account renders");
    let col = full[..byte].chars().count();
    assert_eq!(
        buf.content[row_idx * W as usize + col].bg,
        super::theme::bg_hover(),
        "the blurred pane keeps the last-selected tint:\n{screen}"
    );
}

// #F8: a page press steps a page, not one row. From the top, one PageDown
// lands on the job the S8 rule predicts from hand-computed row starts (17 with
// the seed's 2-line expansion), and PageUp mirrors it back more than one row.
#[test]
fn a_page_press_steps_a_page_not_one_row() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_delegates(seed_walk_jobs(22));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    render(&app); // publish the cursor-0 geometry the page key reads
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(
        app.services.delegates_cursor, 17,
        "one PageDown from the top lands on the predicted job, not one row"
    );
    render(&app); // publish the cursor-17 geometry
    handle_key(&mut app, crate::testutil::key(KeyCode::PageUp));
    assert!(
        app.services.delegates_cursor < 16,
        "one PageUp steps a page back, not one row (cursor={})",
        app.services.delegates_cursor
    );
}

// #F1: the descended footer derives from the count the handler steps over —
// one job advertises no `↑↓`/page group, two or more advertise both.
#[test]
fn the_descended_delegates_footer_derives_from_the_selection_count() {
    use crate::tui::app::ServicesFocus;
    let _home = crate::testutil::HomeSandbox::new();
    let hints = |app: &App| super::super::footer::services_hints(app);

    let mut one = app_with_delegates(seed_walk_jobs(1));
    one.services.focus = ServicesFocus::Detail;
    assert_eq!(
        hints(&one),
        vec![("r", "refresh"), ("a", "actions"), ("?", "help")],
        "a single job cannot move, so no ↑↓ or page group"
    );

    let mut many = app_with_delegates(seed_walk_jobs(22));
    many.services.focus = ServicesFocus::Detail;
    assert_eq!(
        hints(&many),
        vec![
            ("↑↓", "row"),
            ("pgup/pgdn", "read"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "two or more jobs advertise the walking keys"
    );
}

// #N1: on the inline short pane the selected row's caret stays visible on
// every frame — when the last block plus the footnote do not fit, the block
// wins and the footnote stays below the fold.
#[test]
fn the_selected_job_stays_visible_on_the_inline_short_pane() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    // 4 jobs with a 2-line expansion at a 3-row body: inline mode, the last
    // block (3 lines) plus the steer (1) exceed the viewport.
    let mut app = app_with_delegates(seed_walk_jobs(4));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    for _ in 0..4 {
        let rows = render_delegates_detail(&app, 70, 3);
        let screen = rows.join("\n");
        assert!(
            rows.iter().any(|r| r.starts_with('❯')),
            "the selected job's caret stays on screen:\n{screen}"
        );
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
}

// #N1: the inline steer IS reached when the last block plus the footnote fit
// in the viewport together (a 1-line expansion leaves room for the steer).
#[test]
fn the_inline_steer_is_reached_when_the_last_block_plus_it_fits() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    for i in 0..4 {
        jobs::write_heartbeat(
            &RunningSpec {
                idle_secs: None,
                ..running_spec(
                    &format!("d-fit-{i}"),
                    &format!("acct{i}"),
                    now - 10_000 - i as u64,
                    RecordKind::Collectable,
                )
            },
            now - 1_000 - i as u64,
            "working",
        )
        .unwrap();
    }
    let mut app = app_with_delegates(jobs::list_banded(now));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    let mut reached = false;
    for _ in 0..4 {
        let rows = render_delegates_detail(&app, 70, 3);
        if rows.iter().any(|r| r.contains("manage delegates")) {
            reached = true;
        }
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
    assert!(
        reached,
        "the inline steer is reached when the last block plus it fits"
    );
}

// #N2: with zero jobs and a pinned footnote area the empty state renders its
// rounded frame, and the bare fallback shows only where the framed box cannot
// fit.
#[test]
fn the_empty_delegates_detail_keeps_its_frame_when_pinned() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let steer = "manage delegates in clauth app on web or mobile (coming soon)";
    let pad = |s: &str| format!("{s:<70}");
    let boxed = |s: &str| format!("│{s:<68}│");
    let top = format!("╭{}╮", "─".repeat(68));
    let bottom = format!("╰{}╯", "─".repeat(68));
    // 70x6: one steer row and one margin row leave a 4-row scroll area, the
    // smallest that holds the framed box whole.
    let framed_6 = vec![
        top.clone(),
        boxed("no delegates"),
        boxed("r to refresh"),
        bottom.clone(),
        pad(""),
        pad(steer),
    ];
    // 70x5: the scroll area is 3 rows, one short of the box, so the bare
    // lines render and the action line `r to refresh` still shows.
    let bare_5 = vec![
        pad("no delegates"),
        pad("r to refresh"),
        pad(""),
        pad(""),
        pad(steer),
    ];
    let mut framed_12 = vec![top.clone(), boxed("no delegates"), boxed("r to refresh")];
    framed_12.extend(std::iter::repeat_n(boxed(""), 6));
    framed_12.extend([bottom.clone(), pad(""), pad(steer)]);
    for (h, want) in [(6u16, framed_6), (5, bare_5), (12, framed_12)] {
        let mut app = app_with_delegates(Vec::new());
        handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
        assert_eq!(render_delegates_detail(&app, 70, h), want, "70x{h}");
    }

    // 100x24 keeps the frame too.
    let mut app = app_with_delegates(Vec::new());
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let rows = render_delegates_detail(&app, 100, 24);
    assert_eq!(
        rows[0],
        format!("╭{}╮", "─".repeat(98)),
        "100x24 top border"
    );
    assert_eq!(
        rows[21],
        format!("╰{}╯", "─".repeat(98)),
        "100x24 bottom border"
    );
    assert_eq!(rows[23], format!("{steer:<100}"), "100x24 steer row");

    // The smallest height can't hold the 4-row box: the bare lines fall back,
    // and the steer is still carried.
    let mut app = app_with_delegates(Vec::new());
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let rows = render_delegates_detail(&app, 70, 3);
    let screen = rows.join("\n");
    assert!(
        screen.contains("no delegates") && screen.contains("manage delegates"),
        "the bare fallback carries the empty state and the steer:\n{screen}"
    );
}

// #F3: the zero-jobs empty state also carries the steer line on a short pane
// (inline mode), never dropping it.
#[test]
fn the_empty_delegates_detail_keeps_the_steer_on_a_short_pane() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_delegates(Vec::new());
    // No job, so `↵` does not descend; the detail renders the empty state.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let rows = render_delegates_detail(&app, 70, 3);
    let screen = rows.join("\n");
    assert!(
        screen.contains("no delegates"),
        "the empty state renders on the short pane:\n{screen}"
    );
    assert!(
        screen.contains("manage delegates"),
        "the steer line is still carried, never dropped:\n{screen}"
    );
}

// #B2 (strengthened): the render fixture's directory is longer than the row
// width, so the expansion must wrap it, pinned by equality.
#[test]
fn the_selected_delegate_expansion_wraps_a_directory_longer_than_the_row() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    let cwd = "/home/u/a/very/long/run/directory/that/overflows/the/pane/width";
    jobs::write_heartbeat(
        &RunningSpec {
            cwd: Some(cwd.to_string()),
            idle_secs: None,
            ..running_spec("d-wrap-0", "acct", now - 5_000, RecordKind::Collectable)
        },
        now - 1_000,
        "working",
    )
    .unwrap();
    let mut app = app_with_delegates(jobs::list_banded(now));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    let rows = render_delegates_detail(&app, 45, 20);
    let trimmed: Vec<&str> = rows.iter().map(|r| r.trim_end()).collect();
    let screen = trimmed.join("\n");
    // The expansion is the span between the selected (caret) row and the
    // steer line, never selected by an indent prefix that an unselected row
    // would also carry.
    let caret = trimmed
        .iter()
        .position(|r| r.starts_with('❯'))
        .expect("the selected row renders");
    let steer = trimmed
        .iter()
        .position(|r| r.starts_with("manage delegates"))
        .expect("the steer renders");
    let joined: String = trimmed[caret + 1..steer]
        .iter()
        .filter(|r| !r.contains("elapsed"))
        .map(|r| r.trim())
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(
        joined, cwd,
        "the wrapped lines reassemble the whole directory:\n{screen}"
    );
}

/// A live row must band ahead of the finished burst that would crowd it out.
///
/// `jobs::list` orders on the retention anchor, which for a `done` record is its
/// FINISH — so every background job that landed a second ago outranks a blocking
/// run that last spoke twenty seconds ago. On anchor order alone the live row
/// sinks below the fold; the band keeps it first, where it stays visible.
#[test]
fn a_live_delegate_outranks_finished_ones_however_recently_they_landed() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    // The row the whole detail exists for: a blocking run, three minutes in.
    jobs::write_heartbeat(
        &running_spec("d-blk-0", "kerry", now - 180_000, RecordKind::Liveness),
        now - 20_000,
        "still thinking",
    )
    .unwrap();
    // A second live row, anchored NEWER than every finished one. Without it the
    // live row is also the OLDEST record in the store, and a mutant that merely
    // reverses the anchor order bands correctly by accident — the same
    // "the mutant is non-equivalent and the fixture cannot tell" shape this file
    // already records for pdqsort. Interleaved, no monotone reordering of the
    // anchor can produce the banded answer.
    jobs::write_heartbeat(
        &running_spec("d-run-9", "fresh", now - 30_000, RecordKind::Collectable),
        now - 500,
        "just spoke",
    )
    .unwrap();
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..20 {
        std::fs::write(
            dir.join(format!("d-bg-{i}.json")),
            serde_json::to_vec(&serde_json::json!({
                "job_id": format!("d-bg-{i}"),
                "profile": format!("bg{i}"),
                "state": "done",
                "started_at": now - 60_000,
                "done_at": now - 1_000,
                "envelope": { "result": "landed" },
            }))
            .unwrap(),
        )
        .unwrap();
    }

    let app = app_with_delegates(jobs::list_banded(now));
    let (rows, _) = render(&app);
    let screen = rows.join("\n");

    assert!(
        rows.iter().any(|r| r.contains('┊') || r.contains('┃')),
        "fixture control: more delegates than the detail holds, so the list \
         overflows:\n{screen}"
    );
    assert!(
        screen.contains("kerry") && screen.contains("fresh"),
        "and it is never a live one — BOTH band first and stay on screen, \
         whatever their anchors:\n{screen}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("kerry") && r.contains("● blocking")),
        "which still reads as what it is:\n{screen}"
    );
}

/// Within a band the listing's newest-first order has to survive the band sort.
///
/// The fixture INTERLEAVES the two bands, which is what makes it a real
/// permutation rather than a no-op: on an input already grouped by rank, pdqsort
/// short-circuits and an unstable sort returns the same vector, so a same-rank or
/// pre-grouped fixture cannot fail whatever the sort does.
#[test]
fn the_band_sort_keeps_each_band_newest_first() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..20u64 {
        jobs::write_heartbeat(
            &running_spec(
                &format!("d-run-{i:02}"),
                &format!("run{i:02}"),
                now - 900_000,
                RecordKind::Collectable,
            ),
            now - 1_000 - i * 1_000,
            "working",
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("d-dun-{i:02}.json")),
            serde_json::to_vec(&serde_json::json!({
                "job_id": format!("d-dun-{i:02}"),
                "profile": format!("dun{i:02}"),
                "state": "done",
                "started_at": now - 900_000,
                "done_at": now - 1_500 - i * 1_000,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    let order: Vec<String> = super::delegate_cells(&jobs::list_banded(now), now)
        .into_iter()
        .map(|c| c.profile)
        .collect();
    let (live, finished) = order.split_at(20);
    assert!(
        live.iter().all(|n| n.starts_with("run")),
        "the live band comes first, whole: {order:?}"
    );
    let want_live: Vec<String> = (0..20).map(|i| format!("run{i:02}")).collect();
    assert_eq!(live, want_live, "and newest-first inside it");
    let want_done: Vec<String> = (0..20).map(|i| format!("dun{i:02}")).collect();
    assert_eq!(
        finished, want_done,
        "the finished band keeps its own order too, which is what decides which \
         rows the overflow marker swallows",
    );
}

/// An empty store still renders the detail, so the steer line is reachable before
/// anyone has ever run a delegate.
#[test]
fn the_delegates_detail_renders_its_empty_state_with_the_steer_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_delegates(Vec::new());
    let (rows, _) = render(&app);
    let screen = rows.join("\n");

    assert!(
        screen.contains("no delegates"),
        "the empty state renders:\n{screen}"
    );
    assert!(
        screen.contains("manage delegates in clauth app on web or mobile (coming soon)"),
        "and the steer line with it:\n{screen}"
    );
}

/// The delegates row's dot is green while a job runs and dim when none, and a
/// profile's rate-limited delegate traffic never moves it.
#[test]
fn the_delegates_dot_is_green_while_a_job_runs_and_dim_when_none() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    jobs::write_heartbeat(
        &running_spec("d-run-0", "acct", now - 5_000, RecordKind::Collectable),
        now,
        "working",
    )
    .unwrap();
    assert_dot(&delegates_check(&jobs::list_banded(now), &[]), Health::Ok);
    assert_dot(&delegates_check(&[], &[]), Health::Idle);
    assert_dot(&delegates_check(&[], &["acct".to_string()]), Health::Idle);
    assert_dot(
        &delegates_check(&jobs::list_banded(now), &["acct".to_string()]),
        Health::Ok,
    );
}

/// The delegates detail renders the rate-limit warning line above the list when
/// a profile's delegate traffic is rate-limited.
#[test]
fn the_delegates_detail_renders_the_rate_limit_warning() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with(delegates_check(&[], &["acct".to_string()]));
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    assert!(
        screen.contains("rate-limited (acct)"),
        "the warning line renders in the delegates detail:\n{screen}"
    );
}

#[test]
fn the_pane_title_opens_with_the_corner_dash() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_delegates(Vec::new());
    let (rows, _) = render(&app);
    assert!(
        rows.iter().any(|r| r.starts_with("╭─ SERVICES ")),
        "the pane title carries the corner-adjacent dash:\n{:?}",
        rows.iter().take(3).collect::<Vec<_>>()
    );
}

// ── herdr row ──────────────────────────────────────────────────────────────────

#[test]
fn herdr_row_renders_ok_dot_without_fix() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(&healthy_probe(), Some(&healthy_config()));
    assert_dot(&check, Health::Ok);
}

#[test]
fn herdr_row_renders_danger_dot_on_registry_warnings() {
    let _home = crate::testutil::HomeSandbox::new();
    let probe = probe(
        Some("0.8.0"),
        Some(entry(true, None, vec!["plugin root is gone"])),
        None,
    );
    let check = herdr_check(&probe, Some(&healthy_config()));
    assert_dot(&check, Health::Danger);
}

#[test]
fn herdr_row_renders_danger_dot_on_registry_error() {
    let _home = crate::testutil::HomeSandbox::new();
    let probe = probe(
        Some("0.8.0"),
        None,
        Some("herdr's plugin list did not parse"),
    );
    let check = herdr_check(&probe, Some(&healthy_config()));
    assert_dot(&check, Health::Danger);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_not_installed() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(&probe(Some("0.8.0"), None, None), Some(&healthy_config()));
    assert_dot(&check, Health::Warn);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_version_too_old() {
    let _home = crate::testutil::HomeSandbox::new();
    let probe = probe(
        Some("0.7.0"),
        Some(entry(true, Some("0.8.0"), vec![])),
        None,
    );
    let check = herdr_check(&probe, Some(&healthy_config()));
    assert_dot(&check, Health::Warn);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_config_does_not_parse() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(
        &healthy_probe(),
        Some(&config(false, None, SidebarState::Absent)),
    );
    assert_dot(&check, Health::Warn);
}

#[test]
fn herdr_row_renders_warn_dot_and_a_bare_f_line_when_key_unbound() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(
        &healthy_probe(),
        Some(&config(true, None, SidebarState::Templated)),
    );
    assert_dot(&check, Health::Warn);
    assert!(check.detail.iter().any(|l| l == "f  heal herdr config"));
    assert!(!check.detail.iter().any(|l| l.starts_with("[f]")));
}

#[test]
fn herdr_row_renders_warn_dot_and_a_bare_f_line_when_sidebar_untemplated() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(
        &healthy_probe(),
        Some(&config(true, Some("prefix+a"), SidebarState::Absent)),
    );
    assert_dot(&check, Health::Warn);
    assert!(check.detail.iter().any(|l| l == "f  heal herdr config"));
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_config_unreadable() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(&healthy_probe(), None);
    assert_dot(&check, Health::Warn);
}

// ── herdr options ─────────────────────────────────────────────────────────────

/// The herdr detail with its options section: the probe + config verdict the
/// recompute caches, the check built from them, focus descended into the
/// detail so the rows render focusable.
fn herdr_options_app(config: ConfigStatus) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    let probe = healthy_probe();
    app.services.herdr = Some(Some(probe.clone()));
    app.services.herdr_config = Some(config.clone());
    app.services.checks = vec![herdr_check(&probe, Some(&config))];
    app.services.cursor = 0;
    app.services.focus = crate::tui::app::ServicesFocus::Detail;
    app
}

/// The six rows render their real values and glyphs on BOTH tiers — the toggle
/// glyph is the one control the tier changes, so each tier pins its own.
#[test]
fn herdr_options_render_all_six_rows_on_both_tiers() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = herdr_options_app(healthy_config());
    let row_with = |rows: &[String], label: &str| -> String {
        rows.iter()
            .find(|r| r.contains(label))
            .unwrap_or_else(|| panic!("no `{label}` row"))
            .clone()
    };

    let full = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    assert!(screen.contains("OPTIONS"), "the eyebrow renders:\n{screen}");
    assert!(
        row_with(&rows, "popup width").contains("popup width  [fit]  half  split-right  split-top"),
        "the focused cycle row brackets its selection:\n{screen}"
    );
    assert!(
        row_with(&rows, "pane tag").contains("─●"),
        "pane tag on:\n{screen}"
    );
    assert!(
        row_with(&rows, "tag refresh").contains("5s"),
        "tag refresh default 5s:\n{screen}"
    );
    assert!(
        row_with(&rows, "border label").contains("○─"),
        "border label off:\n{screen}"
    );
    assert!(
        row_with(&rows, "delegate dot").contains("─●"),
        "delegate dot on:\n{screen}"
    );
    assert!(
        row_with(&rows, "delegate row text").contains("○─"),
        "delegate row text off:\n{screen}"
    );
    drop(full);

    let compatible = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    assert!(
        row_with(&rows, "pane tag").contains("[on]"),
        "pane tag [on]:\n{screen}"
    );
    assert!(
        row_with(&rows, "border label").contains("[off]"),
        "border label [off]:\n{screen}"
    );
    assert!(
        row_with(&rows, "delegate dot").contains("[on]"),
        "delegate dot [on]:\n{screen}"
    );
    assert!(
        row_with(&rows, "delegate row text").contains("[off]"),
        "delegate row text [off]:\n{screen}"
    );
    drop(compatible);
}

/// The herdr OPTIONS eyebrow takes `TEXT + bold` while the detail pane holds
/// focus on one of its rows, and sits at `TEXT_DIM + bold` while the focus
/// rests on the heal line above the section or the list holds it; it is never
/// underlined.
#[test]
fn the_herdr_options_eyebrow_brightens_only_while_an_option_row_is_focused() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);

    let mut app = herdr_options_app(config(true, None, SidebarState::Templated));
    app.tab = crate::tui::app::Tab::Services;
    let (_, buf) = render(&app);
    assert_eq!(
        app.services.focused_fix(),
        None,
        "an option row holds focus"
    );
    assert_eyebrow(&buf, "OPTIONS", EYEBROW_ACTIVE);

    app.services.focus = ServicesFocus::List;
    let (_, buf) = render(&app);
    assert_eyebrow(&buf, "OPTIONS", EYEBROW_AT_REST);

    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    assert_eq!(app.services.focus, ServicesFocus::Detail, "f descends");
    assert!(
        matches!(
            app.services.focused_fix(),
            Some(ServiceFix::HealHerdrConfig(_))
        ),
        "f lands on the heal line"
    );
    let (_, buf) = render(&app);
    assert_eyebrow(&buf, "OPTIONS", EYEBROW_AT_REST);
}

/// A narrow detail pane breaks the `popup width` cycle between options onto a
/// line indented to the value column, never inside `split-right`.
#[test]
fn herdr_popup_width_row_wraps_between_options_in_a_narrow_pane() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = herdr_options_app(healthy_config());
    let mut term = Terminal::new(TestBackend::new(70, 40)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let screen = rows.join("\n");
    let at = rows
        .iter()
        .position(|r| r.contains("popup width"))
        .unwrap_or_else(|| panic!("no popup width row:\n{screen}"));
    let detail = |r: &str| -> String {
        r.split("││")
            .nth(1)
            .unwrap_or_default()
            .trim_end_matches('│')
            .trim_end()
            .to_string()
    };
    assert_eq!(
        [detail(&rows[at]), detail(&rows[at + 1])],
        [
            " ❯ popup width  [fit]  half  split-right",
            "                split-top"
        ],
        "{screen}"
    );
}

/// While focus sits on the selector, the option rows render blurred: no caret,
/// and the cycle row carries its selection by color alone (no brackets).
#[test]
fn herdr_options_render_blurred_when_focus_sits_on_the_selector() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(healthy_config());
    app.services.focus = crate::tui::app::ServicesFocus::List;
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    let width_row = rows
        .iter()
        .find(|r| r.contains("popup width"))
        .unwrap_or_else(|| panic!("no popup width row:\n{screen}"));
    assert!(
        width_row.contains("popup width  fit  half  split-right  split-top"),
        "a blurred cycle row drops its brackets:\n{screen}"
    );
    assert!(
        !width_row.contains('❯'),
        "the caret renders only inside the focused pane:\n{screen}"
    );
}

/// The `delegate row text` row renders whole-faint with a tooltip while focused
/// when herdr's config does not parse — the one state where the write it would
/// trigger cannot happen. The hue is pinned off the styled buffer, not the
/// glyph text.
#[test]
fn delegate_row_text_renders_inert_with_tooltip_when_herdr_config_does_not_parse() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(config(false, None, SidebarState::Absent));
    app.services.herdr_options_cursor = 5;
    let (rows, buf) = render(&app);
    let screen = rows.join("\n");

    let row_idx = rows
        .iter()
        .position(|r| r.contains("delegate row text"))
        .unwrap_or_else(|| panic!("no delegate row text row:\n{screen}"));
    let row = &rows[row_idx];
    assert!(
        screen.contains("herdr's config doesn't parse, so clauth can't rewrite the row"),
        "the tooltip renders under the focused inert row:\n{screen}"
    );
    for needle in ["❯", "delegate row text"] {
        let byte = row
            .find(needle)
            .unwrap_or_else(|| panic!("no `{needle}`:\n{row}"));
        let col = row[..byte].chars().count();
        assert_eq!(
            buf.content[row_idx * W as usize + col].fg,
            super::theme::text_faint_color(),
            "`{needle}` renders faint on the inert row:\n{screen}"
        );
    }
}

/// The tag-refresh editor renders the edit gutter, the selection-tinted buffer
/// with its unit, and the range sub-line — the Config-tab refresh editor's
/// shape.
#[test]
fn herdr_tag_refresh_editor_renders_the_edit_state() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(healthy_config());
    app.services.herdr_options_cursor = 2;
    app.services.herdr_tag_draft = Some(crate::tui::app::SeededInput::new("5"));
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    let tag_row = rows
        .iter()
        .find(|r| r.contains("tag refresh"))
        .unwrap_or_else(|| panic!("no tag refresh row:\n{screen}"));
    assert!(
        tag_row.contains("✎ tag refresh  5 s"),
        "the edit gutter + buffer + unit render:\n{screen}"
    );
    assert!(
        screen.contains("min is 1 s"),
        "the range sub-line renders while typing:\n{screen}"
    );
}

/// The herdr `tag refresh` editor keeps the selected row's `BG_HOVER` tint over
/// the same cells the row tints at rest, and paints no `BG_SUNKEN` buffer.
#[test]
fn an_editing_herdr_tag_refresh_row_keeps_the_selection_tint() {
    let _home = crate::testutil::HomeSandbox::new();
    for tier in [
        crate::tui::theme::Tier::Full,
        crate::tui::theme::Tier::Compatible,
    ] {
        let _tier = crate::testutil::TierSandbox::new(tier);
        let mut app = herdr_options_app(healthy_config());
        app.services.herdr_options_cursor = 2;
        let tinted = |app: &App| {
            let (rows, buf) = render(app);
            let y = rows
                .iter()
                .position(|r| r.contains("tag refresh"))
                .unwrap_or_else(|| panic!("no tag refresh row:\n{}", rows.join("\n")));
            let hover: Vec<u16> = (0..W)
                .filter(|x| buf[(*x, y as u16)].bg == crate::tui::theme::bg_hover())
                .collect();
            let sunken: Vec<u16> = (0..W)
                .filter(|x| buf[(*x, y as u16)].bg == crate::tui::theme::bg_sunken())
                .collect();
            (hover, sunken)
        };
        let (rest_hover, _) = tinted(&app);
        assert!(
            !rest_hover.is_empty(),
            "the selected row is tinted at rest ({tier:?})"
        );
        app.services.herdr_tag_draft = Some(crate::tui::app::SeededInput::new("5"));
        let (edit_hover, edit_sunken) = tinted(&app);
        assert!(
            edit_sunken.is_empty(),
            "the editor paints no BG_SUNKEN cell ({tier:?})"
        );
        assert_eq!(
            edit_hover, rest_hover,
            "the editing row keeps the selection tint over the same cells ({tier:?})"
        );
    }
}

#[test]
fn herdr_form_keeps_its_selected_row_in_view_across_adjacent_moves_and_resize() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(healthy_config());
    app.tab = crate::tui::app::Tab::Services;
    app.services.herdr_options_cursor = 5;
    let mut term = Terminal::new(TestBackend::new(80, 14)).unwrap();
    term.draw(|f| super::super::draw(f, &app)).unwrap();
    let bottom = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        bottom.iter().any(|r| r.contains("delegate row text")),
        "last option is visible"
    );
    let prior = app.services.form_offset.get();
    assert!(prior > 0, "herdr form has scrolled past its intro");
    app.services.herdr_options_cursor = 4;
    term.draw(|f| super::super::draw(f, &app)).unwrap();
    let adjacent = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        adjacent.iter().any(|r| r.contains("delegate dot")),
        "adjacent option is visible"
    );
    assert!(
        adjacent.iter().any(|r| r.contains("delegate row text")),
        "context below remains visible"
    );
    assert_eq!(
        app.services.form_offset.get(),
        prior,
        "adjacent move keeps the viewport"
    );
    let before_row = bottom
        .iter()
        .position(|r| r.contains("delegate row text"))
        .unwrap();
    let after_row = adjacent
        .iter()
        .position(|r| r.contains("delegate row text"))
        .unwrap();
    assert_eq!(after_row, before_row, "last option stays at its screen row");
    term.backend_mut().resize(80, 45);
    term.autoresize().unwrap();
    term.draw(|f| super::super::draw(f, &app)).unwrap();
    assert_eq!(
        app.services.form_offset.get(),
        0,
        "fitting herdr form resets the offset"
    );
}

// ── shunt card render ────────────────────────────────────────────────────────

fn shunt_record(
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

fn shunt_card_app(
    slot: crate::daemon::gateway::GatewaySlot,
    supervised: bool,
    record: Option<crate::gateway::GatewayRecord>,
    admin_need: Option<crate::gateway::AdminNeed>,
    move_plan: Option<crate::tui::app::MovePlanOutcome>,
    pool: Option<crate::tui::app::PoolOutcome>,
) -> App {
    let card = crate::tui::app::shunt_card(
        &slot,
        supervised,
        false,
        None,
        record.as_ref(),
        admin_need,
        move_plan.as_ref(),
        pool.as_ref(),
        false,
        false,
        false,
    );
    let mut app = app_with(card);
    app.services.shunt_record = record;
    app
}

/// The action section renders verbatim: the MOVE PLAN rows and the pool note
/// keep their literal `: ` (never split into a `key: value` column), and the
/// admin and move fix lines sit directly under their groups.
#[test]
fn the_shunt_card_renders_the_action_section_verbatim() {
    use crate::gateway::{KeptFile, StoreMovePlan};
    let home = crate::testutil::HomeSandbox::new();
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());

    let plan = StoreMovePlan {
        moved: vec![
            crate::gateway::MovedFile {
                from: "/standalone/a.json".into(),
                to: "/clauth/a.json".into(),
            },
            crate::gateway::MovedFile {
                from: "/standalone/b.json".into(),
                to: "/clauth/b.json".into(),
            },
        ],
        kept: vec![KeptFile::NoHome {
            store: "SHUNT_XAI_AUTH_FILE",
        }],
    };
    let app = shunt_card_app(
        slot,
        false,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(crate::tui::app::MovePlanOutcome::Plan(plan)),
        Some(crate::tui::app::PoolOutcome::Providers(vec![
            "codex".to_string(),
            "antigravity".to_string(),
        ])),
    );
    let (rows, _) = render(&app);
    let detail = detail_rows(&rows);
    let screen = rows.join("\n");
    for needle in [
        "MOVE PLAN",
        "moves  /standalone/a.json",
        "moves  /standalone/b.json",
        "f  add admin key",
        "f  move stores in",
        "codex  provider codex needs its own pool login",
        "codex  provider antigravity needs its own pool login",
    ] {
        assert!(
            detail.iter().any(|l| l == needle),
            "`{needle}` renders verbatim:\n{screen}"
        );
    }
    // The NoHome `: ` never becomes a key column: the rendered row keeps the
    // colon (a `key: value` split would consume it into a two-space gap). The
    // store key middle-truncates (the contract's path rule) and the reason
    // stays whole.
    assert!(
        detail
            .iter()
            .any(|l| l == "stays  SHUNT_…H_FILE  no home to find it: set HOME in the env file"),
        "the NoHome store key middle-truncates and its reason stays whole:\n{screen}"
    );
    assert!(
        !detail.iter().any(|l| l == "no home to find it"),
        "the NoHome reason is not split into a key: {detail:?}"
    );
}

/// The MOVE PLAN eyebrow takes `TEXT + bold` while the detail pane holds
/// focus on its `f  move stores in` line, and sits at `TEXT_DIM + bold` while
/// the detail focus rests on another fix line (`f  add admin key`) or the list
/// takes focus back; it is never underlined.
#[test]
fn the_move_plan_eyebrow_brightens_only_while_its_fix_line_is_focused() {
    use crate::tui::app::{ServicesFocus, ShuntFocus, ShuntFocusLine};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let check = Check {
        label: "shunt",
        health: Health::Warn,
        detail: vec![
            "f  add admin key".to_string(),
            "MOVE PLAN".to_string(),
            "moves  /standalone/a.json".to_string(),
            "f  move stores in".to_string(),
        ],
        fix: Some(ServiceFix::MoveStoresIn),
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 0,
                fix: ServiceFix::AddAdminKey,
            },
            ShuntFocus::Fix {
                line: 3,
                fix: ServiceFix::MoveStoresIn,
            },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    app.services.focus = ServicesFocus::Detail;
    app.services.set_shunt_focus(Some(ShuntFocusLine::Move));
    let (_, buf) = render(&app);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Move),
        "the render keeps the move fix line focused"
    );
    assert_eyebrow(&buf, "MOVE PLAN", EYEBROW_ACTIVE);

    app.services.set_shunt_focus(Some(ShuntFocusLine::Admin));
    let (_, buf) = render(&app);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Admin),
        "the render keeps the admin fix line focused"
    );
    assert_eyebrow(&buf, "MOVE PLAN", EYEBROW_AT_REST);

    app.services.set_shunt_focus(Some(ShuntFocusLine::Move));
    app.services.focus = ServicesFocus::List;
    let (_, buf) = render(&app);
    assert_eyebrow(&buf, "MOVE PLAN", EYEBROW_AT_REST);
}

/// The pending-require waiting line renders whole-dim (a read-only line, never
/// the label/body split a live `key value` row gets), and carries no focus
/// stop — a plant dropping the whole-dim branch (or making it a stop) reds.
#[test]
fn the_pending_require_waiting_line_renders_dim_and_read_only() {
    let home = crate::testutil::HomeSandbox::new();
    let record = shunt_record(&home, false);
    let slot = shunt_slot(crate::daemon::gateway::GatewayState::Healthy);
    let card = crate::tui::app::shunt_card(
        &slot,
        true,
        false,
        None,
        Some(&record),
        None,
        None,
        None,
        false,
        true,
        true,
    );
    let waiting = crate::tui::app::pending_require_line();
    let line = card
        .detail
        .iter()
        .position(|l| l == &waiting)
        .expect("the waiting line renders in the detail");
    assert!(
        card.shunt_focus.iter().all(|f| f.line() != Some(line)),
        "the waiting line is read-only: no focus stop"
    );
    let app = app_with(card);
    let (_, buf) = render(&app);
    let cells = crate::testutil::needle_cells(&buf, "waiting for the gateway to restart")
        .unwrap_or_else(|| panic!("the waiting line renders"));
    assert!(
        cells.iter().all(|c| c.fg == super::theme::text_dim_color()),
        "the waiting line renders whole-dim"
    );
}

/// The `enabled` row renders the tier's toggle glyph, on and off, off the
/// cached record.
#[test]
fn the_shunt_card_renders_the_enabled_toggle_on_and_off() {
    let home = crate::testutil::HomeSandbox::new();
    let on = shunt_record(&home, false);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Healthy);
    slot.config = Some(on.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let app = shunt_card_app(slot.clone(), true, Some(on), None, None, None);
    let (rows, _) = render(&app);
    assert!(
        rows.iter()
            .any(|r| r.contains(&format!("enabled  {}", super::theme::toggle_on()))),
        "on: {}",
        rows.join("\n")
    );

    let off = shunt_record(&home, true);
    slot.state = crate::daemon::gateway::GatewayState::Disabled;
    slot.config = Some(off.config().display().to_string());
    let app = shunt_card_app(slot, true, Some(off), None, None, None);
    let (rows, _) = render(&app);
    assert!(
        rows.iter()
            .any(|r| r.contains(&format!("enabled  {}", super::theme::toggle_off()))),
        "off: {}",
        rows.join("\n")
    );
}

/// The `pool` failure line renders as the one warn line in the note's place.
#[test]
fn the_shunt_card_renders_the_pool_failure_line() {
    let home = crate::testutil::HomeSandbox::new();
    let record = shunt_record(&home, false);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Healthy);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let app = shunt_card_app(
        slot,
        true,
        Some(record),
        Some(crate::gateway::AdminNeed::Neither),
        None,
        Some(crate::tui::app::PoolOutcome::Error(
            "the gateway's pool response does not parse".to_string(),
        )),
    );
    let (rows, _) = render(&app);
    assert!(
        detail_rows(&rows)
            .iter()
            .any(|l| l == "pool  the gateway's pool response does not parse"),
        "{}",
        rows.join("\n")
    );
}

/// P6: the `pool` failure line's value renders in the warning tone, and the
/// `refused` line matches it — an error line, not body prose.
#[test]
fn the_shunt_card_renders_the_pool_and_refused_lines_in_the_warning_tone() {
    let home = crate::testutil::HomeSandbox::new();
    let record = shunt_record(&home, false);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Healthy);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let app = shunt_card_app(
        slot,
        true,
        Some(record),
        Some(crate::gateway::AdminNeed::Neither),
        None,
        Some(crate::tui::app::PoolOutcome::Error(
            "the gateway's pool response does not parse".to_string(),
        )),
    );
    let (rows, buf) = render(&app);
    let row_idx = rows
        .iter()
        .position(|r| r.contains("pool  "))
        .unwrap_or_else(|| panic!("no pool row:\n{}", rows.join("\n")));
    let row = &rows[row_idx];
    let value_col = row
        .find("pool  ")
        .map(|b| row[..b].chars().count() + "pool  ".chars().count())
        .unwrap();
    assert_eq!(
        buf.content[row_idx * W as usize + value_col].fg,
        super::theme::warning_color(),
        "the pool value is warning-toned:\n{}",
        rows.join("\n")
    );
}

/// The `refused` and `pool` sentences wrap within the pane instead of
/// truncating, so the fix instruction is never cut.
#[test]
fn the_shunt_card_wraps_the_refused_and_pool_sentences() {
    let home = crate::testutil::HomeSandbox::new();
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let app = shunt_card_app(
        slot,
        false,
        Some(record),
        None,
        Some(crate::tui::app::MovePlanOutcome::Refused(
            "the standalone's home names no absolute directory, so clauth cannot tell where the codex CLI's own login (~/.codex/auth.json) sits; nothing was moved; set HOME to an absolute path in the env file, then run the move again".to_string(),
        )),
        None,
    );
    let (rows, _) = render(&app);
    let detail = detail_rows(&rows);
    // The sentence wraps within the pane instead of truncating: no trailing
    // ellipsis on the `refused` row, and the fix instruction survives whole to
    // the last wrapped row.
    assert!(
        !detail
            .iter()
            .any(|l| l.starts_with("refused") && l.ends_with('…')),
        "the refused sentence wraps instead of truncating:\n{}",
        rows.join("\n")
    );
    assert!(
        detail
            .iter()
            .any(|l| l.ends_with("then run the move again")),
        "the fix instruction is never cut:\n{}",
        rows.join("\n")
    );
}

/// Ruling 1's acceptance render: a refused plan with an admin step, scrolled to
/// its last line on an 80x20 terminal and at the narrow stacked width the
/// review measured (58x24), shows the refusal's last words. ↑↓ pages the card
/// one line at a time (the render clamps the scroll), so the read-only tail
/// below the last actionable line is reachable. The supervised field set makes
/// the card overflow both viewports, so the scroll is what reaches the tail.
#[test]
fn the_refused_plans_last_line_is_reachable_at_80x20_and_58x24() {
    use crate::tui::app::{MovePlanOutcome, ServicesFocus};
    let home = crate::testutil::HomeSandbox::new();
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    slot.version = Some("0.49.1".to_string());
    slot.reason = Some("port busy".to_string());
    slot.pid = Some(4242);
    slot.port = Some(3001);
    slot.restarts = 2;
    slot.last_exit = Some(crate::daemon::gateway::ExitReport {
        code: Some(1),
        signal: None,
    });
    let refusal = "the standalone's home names no absolute directory, so clauth cannot tell where the codex CLI's own login (~/.codex/auth.json) sits; nothing was moved; set HOME to an absolute path in the env file, then run the move again".to_string();

    for (w, h) in [(80u16, 20u16), (58u16, 24u16)] {
        let mut app = shunt_card_app(
            slot.clone(),
            true,
            Some(record.clone()),
            Some(crate::gateway::AdminNeed::WriteKey),
            Some(MovePlanOutcome::Refused(refusal.clone())),
            None,
        );
        app.services.focus = ServicesFocus::Detail;
        // The render clamps against `detail_max_scroll`; a huge value pins the
        // card scrolled to its last line.
        app.services.shunt_scroll.set(usize::MAX);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| super::super::draw(f, &app)).unwrap();
        let screen = crate::testutil::buffer_rows(term.backend().buffer()).join("\n");
        assert!(
            screen.contains("move again"),
            "the refusal's last words are reachable at {w}x{h}:\n{screen}"
        );
    }
}

/// While a shunt action runs, the `enabled` row and the shunt `f` lines render
/// whole-faint (the disabled-row treatment) — inert, matching the key handlers
/// that return early and the footer that drops their hints.
#[test]
fn the_shunt_action_renders_the_enabled_and_fix_rows_inert() {
    let home = crate::testutil::HomeSandbox::new();
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let mut app = shunt_card_app(
        slot,
        false,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        None,
        None,
    );
    app.services.shunt_action.running = true;
    let (rows, buf) = render(&app);
    let screen = rows.join("\n");

    let enabled_row = rows
        .iter()
        .position(|r| r.contains("enabled  "))
        .unwrap_or_else(|| panic!("no enabled row:\n{screen}"));
    let col = rows[enabled_row]
        .find("enabled")
        .map(|b| rows[enabled_row][..b].chars().count())
        .unwrap();
    assert_eq!(
        buf.content[enabled_row * W as usize + col].fg,
        super::theme::text_faint_color(),
        "the enabled row is inert (faint):\n{screen}"
    );

    let fix_row = rows
        .iter()
        .position(|r| r.contains("add admin key"))
        .unwrap_or_else(|| panic!("no fix row:\n{screen}"));
    let col = rows[fix_row]
        .find("f")
        .map(|b| rows[fix_row][..b].chars().count())
        .unwrap();
    assert_eq!(
        buf.content[fix_row * W as usize + col].fg,
        super::theme::text_faint_color(),
        "the fix line is inert (faint):\n{screen}"
    );
}

/// The toggle renders the theme's own glyph ([`super::theme::toggle_on`] /
/// [`super::theme::toggle_off`]) on both tiers, so a glyph change in the theme
/// reaches the `enabled` row and the herdr toggles with no second copy to edit.
#[test]
fn the_enabled_row_renders_the_theme_toggle_glyph() {
    let home = crate::testutil::HomeSandbox::new();
    let on = shunt_record(&home, false);
    let off = shunt_record(&home, true);
    for tier in [super::theme::Tier::Full, super::theme::Tier::Compatible] {
        let _tier = crate::testutil::TierSandbox::new(tier);
        for (record, enabled) in [(&on, true), (&off, false)] {
            let mut slot = shunt_slot(if enabled {
                crate::daemon::gateway::GatewayState::Healthy
            } else {
                crate::daemon::gateway::GatewayState::Disabled
            });
            slot.config = Some(record.config().display().to_string());
            slot.binary = Some("shunt".to_string());
            let app = shunt_card_app(slot, true, Some(record.clone()), None, None, None);
            let (rows, _) = render(&app);
            let screen = rows.join("\n");
            let row = rows
                .iter()
                .find(|r| r.contains("enabled  "))
                .unwrap_or_else(|| panic!("no enabled row:\n{screen}"));
            let glyph = if enabled {
                super::theme::toggle_on()
            } else {
                super::theme::toggle_off()
            };
            let value = &row[row.find("enabled  ").unwrap() + "enabled  ".len()..];
            assert!(
                value.starts_with(glyph),
                "{tier:?} enabled={enabled}: the row renders {glyph:?}:\n{screen}"
            );
        }
    }
}

/// S11: the toggle paints its track, knob and brackets in the contract's
/// separate colours — `full` track `LINE` + knob `ACCENT`/`TEXT_DIM`;
/// `compatible` brackets `TEXT_DIM` + `on` `ACCENT` / `off` `TEXT_DIM`. The one
/// helper feeds the shunt `enabled` row and the herdr toggles, so both change
/// together. Both tiers are pinned explicitly under a `TierSandbox`, so a plant
/// on either arm reds this test on any box.
#[test]
fn the_toggle_paints_its_parts_in_the_contract_colours() {
    let home = crate::testutil::HomeSandbox::new();
    let on = shunt_record(&home, false);
    let off = shunt_record(&home, true);
    for tier in [super::theme::Tier::Full, super::theme::Tier::Compatible] {
        let _tier = crate::testutil::TierSandbox::new(tier);

        for (record, enabled) in [(&on, true), (&off, false)] {
            let mut slot = shunt_slot(if enabled {
                crate::daemon::gateway::GatewayState::Healthy
            } else {
                crate::daemon::gateway::GatewayState::Disabled
            });
            slot.config = Some(record.config().display().to_string());
            slot.binary = Some("shunt".to_string());
            let app = shunt_card_app(slot, true, Some(record.clone()), None, None, None);
            let (rows, buf) = render(&app);
            let screen = rows.join("\n");
            let row_idx = rows
                .iter()
                .position(|r| r.contains("enabled  "))
                .unwrap_or_else(|| panic!("no enabled row:\n{screen}"));
            let row = &rows[row_idx];
            let base = row
                .find("enabled")
                .map(|b| row[..b].chars().count() + "enabled  ".chars().count())
                .unwrap();
            let fg = |col: usize| buf.content[row_idx * W as usize + col].fg;
            match tier {
                super::theme::Tier::Full => {
                    if enabled {
                        assert_eq!(
                            fg(base),
                            super::theme::line_color(),
                            "track is LINE:\n{screen}"
                        );
                        assert_eq!(
                            fg(base + 1),
                            super::theme::accent_color(),
                            "on knob is ACCENT:\n{screen}"
                        );
                    } else {
                        assert_eq!(
                            fg(base),
                            super::theme::text_dim_color(),
                            "off knob is TEXT_DIM:\n{screen}"
                        );
                        assert_eq!(
                            fg(base + 1),
                            super::theme::line_color(),
                            "track is LINE:\n{screen}"
                        );
                    }
                }
                super::theme::Tier::Compatible => {
                    if enabled {
                        assert_eq!(
                            fg(base),
                            super::theme::text_dim_color(),
                            "bracket is TEXT_DIM:\n{screen}"
                        );
                        assert_eq!(
                            fg(base + 1),
                            super::theme::accent_color(),
                            "on word is ACCENT:\n{screen}"
                        );
                        assert_eq!(
                            fg(base + 3),
                            super::theme::text_dim_color(),
                            "closing bracket is TEXT_DIM:\n{screen}"
                        );
                    } else {
                        assert_eq!(
                            fg(base),
                            super::theme::text_dim_color(),
                            "bracket is TEXT_DIM:\n{screen}"
                        );
                        assert_eq!(
                            fg(base + 1),
                            super::theme::text_dim_color(),
                            "off word is TEXT_DIM:\n{screen}"
                        );
                        assert_eq!(
                            fg(base + 4),
                            super::theme::text_dim_color(),
                            "closing bracket is TEXT_DIM:\n{screen}"
                        );
                    }
                }
            }
        }
    }
}

// ── shunt focus walk ────────────────────────────────────────────────────────

/// The full frame at `w`×`h` (header + body + footer), drawn once so the
/// render publishes the geometry the shunt key handler walks over.
fn draw_frame(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::super::draw(f, app)).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
}

/// Which actionable line holds the caret in the shunt detail pane, as a
/// canonical word — the equality pin for the ↓/↑ walks.
fn caret_stop(rows: &[String]) -> Option<&'static str> {
    let line = rows
        .iter()
        .filter_map(|r| r.split("││").nth(1))
        .find(|d| d.contains('❯'))?;
    let line = line.trim_matches('│').trim();
    if line.contains("add admin key") {
        Some("admin")
    } else if line.contains("move stores in") {
        Some("move")
    } else if line.contains("adopt config") {
        Some("adopt")
    } else if line.contains("enabled") {
        Some("enabled")
    } else {
        None
    }
}

/// A supervised, disabled, adopted card whose standalone move plan moves three
/// files, and whose admin key is owed — the 120x24 RIGHT-walk fixture (three
/// actionable lines: `enabled`, `f  add admin key`, `f  move stores in`).
fn adopted_three_file_card(
    home: &crate::testutil::HomeSandbox,
) -> (
    crate::daemon::gateway::GatewaySlot,
    crate::gateway::GatewayRecord,
    crate::gateway::StoreMovePlan,
) {
    let record = shunt_record(home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    slot.version = Some("0.49.1".to_string());
    let plan = crate::gateway::StoreMovePlan {
        moved: vec![
            crate::gateway::MovedFile {
                from: "/home/u/.shunt/accounts/claude/main.json".into(),
                to: "/home/u/.clauth/shunt/accounts/claude/main.json".into(),
            },
            crate::gateway::MovedFile {
                from: "/home/u/.shunt/xai-auth.json".into(),
                to: "/home/u/.clauth/shunt/xai-auth.json".into(),
            },
            crate::gateway::MovedFile {
                from: "/home/u/.shunt/grok.json".into(),
                to: "/home/u/.clauth/shunt/grok.json".into(),
            },
        ],
        kept: vec![],
    };
    (slot, record, plan)
}

/// ↓/↑ walk the shunt card's actionable lines, landing on each as it comes
/// into view: on a card that fits its pane, descend → `enabled`, ↓ → the admin
/// fix, ↓ → `move stores in`, ↓ → nothing (the card fits), ↑ → back to the
/// admin fix. Pinned by equality on which line holds the caret after each
/// press, on the full frame.
#[test]
fn down_up_walk_the_shunt_stops_on_a_fitting_card() {
    use crate::tui::app::{MovePlanOutcome, ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let (slot, record, plan) = adopted_three_file_card(&home);
    let mut app = shunt_card_app(
        slot,
        false,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Plan(plan)),
        None,
    );

    // The first frame publishes the geometry; then Enter descends.
    let _ = draw_frame(&app, 120, 24);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("enabled"));

    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("admin"));

    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("move"));

    // ↓ at the last stop on a fitting card moves nothing.
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("move"));

    // ↑ mirrors the walk.
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("admin"));
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("enabled"));
}

/// On an 80x20 refused plan with an admin step, ↓ first moves focus to the
/// admin fix, then scrolls one rendered line at a time through the MOVE PLAN
/// and the wrapped refusal to its last words, focus staying on the admin fix
/// while it is in view (at 80x20 it never leaves — the fix sits high enough
/// that the whole scroll range keeps it visible).
#[test]
fn down_scrolls_the_refused_card_one_line_each_to_its_last_words() {
    use crate::tui::app::{MovePlanOutcome, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    slot.version = Some("0.49.1".to_string());
    slot.reason = Some("port busy".to_string());
    slot.pid = Some(4242);
    slot.port = Some(3001);
    slot.restarts = 2;
    slot.last_exit = Some(crate::daemon::gateway::ExitReport {
        code: Some(1),
        signal: None,
    });
    let refusal = "the standalone's home names no absolute directory, so clauth cannot tell where the codex CLI's own login (~/.codex/auth.json) sits; nothing was moved; set HOME to an absolute path in the env file, then run the move again".to_string();
    let mut app = shunt_card_app(
        slot,
        true,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Refused(refusal)),
        None,
    );

    let _ = draw_frame(&app, 80, 20);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(caret_stop(&draw_frame(&app, 80, 20)), Some("enabled"));

    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(caret_stop(&draw_frame(&app, 80, 20)), Some("admin"));

    let max = usize::from(app.services.detail_max_scroll.get());
    assert!(max > 0, "the refused card overflows 80x20");
    // Landing on the admin fix pads `SCROLL_PAD` rows below it (the render's
    // follow), so the landing itself advances the scroll to exactly one row
    // below the top; each further ↓ scrolls exactly one rendered line through
    // the prose below while the caret stays on the admin fix (the card keeps it
    // inside the padded band the whole way).
    let landing = app.services.shunt_scroll.get();
    assert_eq!(
        landing, 1,
        "the ↓ onto the admin fix lands one line down (its block end 13 + pad 2 - viewport 14)"
    );
    for _ in landing..max {
        let before = app.services.shunt_scroll.get();
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        assert_eq!(
            app.services.shunt_scroll.get(),
            before + 1,
            "one line per press"
        );
        assert_eq!(
            caret_stop(&draw_frame(&app, 80, 20)),
            Some("admin"),
            "focus stays on the admin fix while in the padded band"
        );
    }
    let screen = draw_frame(&app, 80, 20).join("\n");
    assert!(
        screen.contains("move again"),
        "the refusal's last words are reachable:\n{screen}"
    );
    // At the end, ↓ moves nothing.
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        app.services.shunt_scroll.get(),
        max,
        "the scroll stays at the end"
    );
}

/// `↵` on the focused `move stores in` line — reachable only by walking, even
/// while an admin fix is also owed — opens the move confirm.
#[test]
fn enter_on_the_focused_move_line_opens_the_move_confirm() {
    use crate::tui::app::{ConfirmAction, Modal, MovePlanOutcome, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let (slot, record, plan) = adopted_three_file_card(&home);
    let mut app = shunt_card_app(
        slot,
        false,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Plan(plan.clone())),
        None,
    );
    // `open_move_confirm` reads the cached plan.
    app.services.move_plan = Some(MovePlanOutcome::Plan(plan));

    let _ = draw_frame(&app, 120, 24);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("move"));

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => assert!(
            matches!(state.on_confirm, ConfirmAction::MoveStoresIn(_)),
            "↵ on the focused move line opens the move confirm, got {:?}",
            state.on_confirm
        ),
        other => panic!("expected the move confirm, got {other:?}"),
    }
}

/// The shunt detail's `↑↓` hint is derived from what a press does: the house
/// walk word (`row`) while a press would move focus, the scroll word (`scroll`)
/// while it would only scroll, and no hint at all on a fitting one-stop card
/// (audit N4). Pinned at the hint-list level.
#[test]
fn the_shunt_detail_hint_names_what_updown_does() {
    use crate::tui::app::ServicesFocus;
    let home = crate::testutil::HomeSandbox::new();
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let hints = |app: &App| super::super::footer::services_hints(app);

    // Two stops (enabled + the admin fix): a press moves focus → `row`.
    let mut app = shunt_card_app(
        slot.clone(),
        false,
        Some(record.clone()),
        Some(crate::gateway::AdminNeed::WriteKey),
        None,
        None,
    );
    app.services.focus = ServicesFocus::Detail;
    assert_eq!(hints(&app)[0], ("↑↓", "row"), "{:?}", hints(&app));

    // One stop (the enabled row) on a card that overflows → `scroll`.
    let mut app = shunt_card_app(slot, false, Some(record), None, None, None);
    app.services.focus = ServicesFocus::Detail;
    app.services.detail_max_scroll.set(3);
    assert_eq!(hints(&app)[0], ("↑↓", "scroll"), "{:?}", hints(&app));

    // One stop, fitting (max scroll 0): no `↑↓` at all.
    app.services.detail_max_scroll.set(0);
    app.services.shunt_scroll.set(0);
    let h = hints(&app);
    assert!(
        h.iter().all(|(k, _)| *k != "↑↓"),
        "a fitting one-stop card hides ↑↓: {h:?}"
    );
}

/// A resize that pushes the focused `move stores in` line off the fold follows
/// it back into view (the render's per-draw follow, F1's class): the caret
/// stays on `move` with two rows past it, never re-settled to `enabled`.
#[test]
fn a_resize_pushing_focus_off_screen_keeps_the_followed_focus() {
    use crate::tui::app::{MovePlanOutcome, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let (slot, record, plan) = adopted_three_file_card(&home);
    // The record's config names an `[server.admin]` table, so the recompute
    // after `space` re-derives the same `WriteKey` admin fix the fixture
    // builds with (`f  add admin key`, not `f  add admin table`).
    std::fs::write(record.config(), "[server]\n\n[server.admin]\n").unwrap();
    // The record on disk (`disabled`), so `space` flips it for real.
    crate::gateway::GatewayRecord::update(|s| {
        *s = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let mut app = shunt_card_app(
        slot,
        false,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Plan(plan.clone())),
        None,
    );
    // `open_move_confirm` reads the cached plan, so a buggy `f` would open it.
    app.services.move_plan = Some(MovePlanOutcome::Plan(plan));

    let _ = draw_frame(&app, 120, 24);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("move"));

    // Resize to 120x14: the move line leaves the fold, and the render's follow
    // brings it back — the caret stays on `move`, two rows past it while the
    // card has them.
    let _ = draw_frame(&app, 120, 14);
    let f = shunt_walk_frame(&app, 120, 14);
    assert!(
        f.pane[f.caret].contains("move stores in"),
        "the caret stays on move:\n{}",
        f.pane.join("\n")
    );
    let below = f.pane.len() - 1 - f.caret;
    let left = f.total - 1 - f.row;
    assert!(
        below >= left.min(2),
        "{below} rows below the followed move line, {left} left:\n{}",
        f.pane.join("\n")
    );
}

/// A card change that pushes the focused `enabled` row — and every actionable
/// line below it — off the fold is followed by the render (F1's class): the
/// focused block is kept in view with two rows past it, so `space` still
/// toggles the row the caret holds.
#[test]
fn a_card_change_pushing_every_stop_off_screen_keeps_the_followed_focus() {
    use crate::tui::app::{MovePlanOutcome, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let (slot, record, plan) = adopted_three_file_card(&home);
    // The record on disk (`disabled`), so `space` flips it for real.
    crate::gateway::GatewayRecord::update(|s| {
        *s = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let mut app = shunt_card_app(
        slot.clone(),
        false,
        Some(record.clone()),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Plan(plan.clone())),
        None,
    );

    let _ = draw_frame(&app, 120, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 14)), Some("enabled"));

    // A worker landing adds a multi-line reason above `enabled`, pushing it
    // and every stop below it out of view; the render's follow keeps the
    // focused block in view.
    let mut grown = slot;
    grown.reason = Some(
        "the daemon could not start the gateway\nbecause the bind was already held\nby another process\nretrying later".to_string(),
    );
    app.services.checks = vec![crate::tui::app::shunt_card(
        &grown,
        false,
        false,
        None,
        Some(&record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(&MovePlanOutcome::Plan(plan)),
        None,
        false,
        false,
        false,
    )];

    let _ = draw_frame(&app, 120, 14);
    assert_eq!(caret_stop(&draw_frame(&app, 120, 14)), Some("enabled"));
    let hints = super::super::footer::services_hints(&app);
    assert!(
        hints.iter().any(|(k, v)| *k == "space/↵" && *v == "toggle"),
        "the followed enabled row offers its toggle verb: {hints:?}"
    );

    // `space` flips the followed `enabled` row, which the follow kept in view.
    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        !crate::gateway::GatewayRecord::load()
            .unwrap()
            .unwrap()
            .disabled,
        "space flips the followed enabled row"
    );
}

/// Re-descending keeps the held focus identity while its line is in view: walk
/// to `move`, ascend, descend again — the caret is back on `move`, not reset to
/// the first actionable line.
#[test]
fn redescending_keeps_the_held_focus_when_its_line_is_in_view() {
    use crate::tui::app::{MovePlanOutcome, ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let (slot, record, plan) = adopted_three_file_card(&home);
    let mut app = shunt_card_app(
        slot,
        false,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Plan(plan)),
        None,
    );

    let _ = draw_frame(&app, 120, 24);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("move"));

    // Ascend: the list takes focus, the identity survives.
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert_eq!(app.services.focus, ServicesFocus::List);

    // Descend again: the held identity's line is in view, so it is kept.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("move"));
}

/// Ascending out of the shunt detail keeps the card's focused-line highlight
/// (the blurred-pane rule) but drops the detail caret and bold, so exactly one
/// caret (the list's) is on screen; the identity survives the ascend.
#[test]
fn ascending_keeps_the_card_highlight_but_drops_the_detail_caret() {
    use crate::tui::app::{MovePlanOutcome, ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let (slot, record, plan) = adopted_three_file_card(&home);
    let mut app = shunt_card_app(
        slot,
        false,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Plan(plan)),
        None,
    );

    let _ = draw_frame(&app, 120, 24);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("enabled"));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("admin"));

    // Ascend: the list takes focus; the detail keeps the admin line's hover
    // tint but draws no caret and no bold.
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert_eq!(app.services.focus, ServicesFocus::List);
    let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
    term.draw(|f| super::super::draw(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let rows = crate::testutil::buffer_rows(&buf);
    let screen = rows.join("\n");
    assert_eq!(
        screen.matches('❯').count(),
        1,
        "exactly one caret (the list's) after ascending:\n{screen}"
    );
    // The admin line still carries the hover tint (blurred-pane rule), pinned
    // off the styled buffer.
    let row_idx = rows
        .iter()
        .position(|r| r.contains("f  add admin key"))
        .expect("the admin line renders");
    let row = &rows[row_idx];
    let byte = row.find("f  add").expect("the admin fix glyph");
    let col = row[..byte].chars().count();
    assert_eq!(
        buf.content[row_idx * 120 + col].bg,
        super::theme::bg_hover(),
        "the blurred card keeps the focused line's highlight:\n{screen}"
    );
}

/// A card change that pushes the focused block off the fold — and the change
/// that brings the stops back — both keep the focus on `enabled` via the
/// render's follow (F1's class); `↓` then walks from it to the admin fix,
/// never skipping `enabled`'s successor.
#[test]
fn bringing_the_stops_back_into_view_keeps_the_followed_focus_then_walks_from_it() {
    use crate::tui::app::{MovePlanOutcome, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let (slot, record, plan) = adopted_three_file_card(&home);
    crate::gateway::GatewayRecord::update(|s| {
        *s = Some(record.clone());
        Ok(())
    })
    .unwrap();
    let mut app = shunt_card_app(
        slot.clone(),
        false,
        Some(record.clone()),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Plan(plan.clone())),
        None,
    );

    let _ = draw_frame(&app, 120, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 14)), Some("enabled"));

    // A worker landing adds a multi-line reason above `enabled`, pushing every
    // stop out of view; the render's follow keeps the focused block in view.
    let mut grown = slot.clone();
    grown.reason = Some(
        "the daemon could not start the gateway\nbecause the bind was already held\nby another process\nretrying later".to_string(),
    );
    app.services.checks = vec![crate::tui::app::shunt_card(
        &grown,
        false,
        false,
        None,
        Some(&record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(&MovePlanOutcome::Plan(plan.clone())),
        None,
        false,
        false,
        false,
    )];
    let _ = draw_frame(&app, 120, 14);
    assert_eq!(caret_stop(&draw_frame(&app, 120, 14)), Some("enabled"));

    // The reason clears (the worker landed): the followed `enabled` stays
    // focused, and ↓ walks from it to the admin fix.
    app.services.checks = vec![crate::tui::app::shunt_card(
        &slot,
        false,
        false,
        None,
        Some(&record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(&MovePlanOutcome::Plan(plan)),
        None,
        false,
        false,
        false,
    )];
    let _ = draw_frame(&app, 120, 14);
    assert_eq!(caret_stop(&draw_frame(&app, 120, 14)), Some("enabled"));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 14)), Some("admin"));
}

/// A held stop that LEAVES the card (a landed token remove recomputes the card
/// without it) re-settles to the first stop in the padded band instead of
/// keeping a stale identity whose line no longer renders (the held-focus
/// branch of the render's re-settle).
#[test]
fn a_vanished_held_stop_re_settles_to_the_first_stop_in_the_band() {
    use crate::tui::app::{ShuntDrafts, ShuntTokens, config_field_stops, handle_key, shunt_check};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = collapsed_upstreams(8);
    let tokens = ShuntTokens {
        stored: vec!["alice".to_string(), "bob".to_string()],
        ..ShuntTokens::default()
    };
    let mut app =
        config_view_app_with_tokens(view.clone(), std::collections::HashSet::new(), tokens);
    let _ = draw_frame(&app, 100, 16);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    for _ in 0..60 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let Some(f) = shunt_walk_frame_focused(&app, 100, 16) else {
            continue;
        };
        if f.pane[f.caret].contains("alice") {
            break;
        }
    }
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(crate::tui::app::ShuntFocusLine::Token("alice".to_string())),
        "the ↓ walk holds the alice token"
    );

    // The token remove lands: the recompute drops alice's stop while the held
    // focus still names it.
    let drafts = ShuntDrafts::default();
    let shrunken = ShuntTokens {
        stored: vec!["bob".to_string()],
        ..ShuntTokens::default()
    };
    let mut card = shunt_check(&shunt_slot_full(), true, None);
    card.shunt_focus.extend(config_field_stops(
        &view,
        &std::collections::HashSet::new(),
        &drafts,
        &shrunken,
    ));
    app.services.checks = vec![card];
    app.services.shunt_tokens = shrunken;
    app.services.shunt_drafts = drafts;
    let _ = draw_frame(&app, 100, 16);
    let settled = app
        .services
        .focused_shunt_focus()
        .map(|s| s.kind())
        .expect("the vanished stop re-settles to a stop in view");
    assert_eq!(
        settled,
        crate::tui::app::ShuntFocusLine::Add(crate::tui::app::ShuntAddKind::Upstream),
        "the vanished stop re-settles to the first stop in the padded band"
    );
    // The re-settle lands inside the band, so the render's follow has nothing
    // to move on the next draw.
    let settled_scroll = app.services.shunt_scroll.get();
    let _ = draw_frame(&app, 100, 16);
    assert_eq!(
        app.services.shunt_scroll.get(),
        settled_scroll,
        "the next draw leaves the view put after the re-settle"
    );
}

/// From a none focus (with stops in view but no re-settle draw in between),
/// `↓` lands on the first stop in view — never skipping it — and `↑` lands on
/// the last stop in view. The re-settle usually fixes a none focus at the next
/// draw; this pins the key handler's own no-skip rule on the same path.
#[test]
fn down_from_none_lands_on_the_first_stop_in_view() {
    use crate::tui::app::{MovePlanOutcome, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let (slot, record, plan) = adopted_three_file_card(&home);
    let mut app = shunt_card_app(
        slot,
        false,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Plan(plan)),
        None,
    );

    let _ = draw_frame(&app, 120, 24);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("enabled"));

    // Clear the focus with no draw in between: the next ↓ starts from none and
    // lands on the first stop in view, never skipping it.
    app.services.set_shunt_focus(None);
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("enabled"));

    // ↑ from none lands on the last stop in view.
    app.services.set_shunt_focus(None);
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(caret_stop(&draw_frame(&app, 120, 24)), Some("move"));
}

/// On a short pane, `↓` on the card's only stop scrolls it out the top: focus
/// stays none while no stop is in view (round 4's rule), and the next frame
/// with a stop in view re-settles focus to it — no key press.
#[test]
fn scrolling_past_the_last_stop_leaves_focus_none_until_a_stop_returns_to_view() {
    use crate::tui::app::{PoolOutcome, ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    let mut app = shunt_card_app(
        slot,
        false,
        Some(record),
        None,
        None,
        Some(PoolOutcome::Providers(vec![
            "openai".to_string(),
            "anthropic".to_string(),
            "deepseek".to_string(),
            "zai".to_string(),
            "alibaba".to_string(),
            "openrouter".to_string(),
        ])),
    );

    let _ = draw_frame(&app, 120, 12);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    assert_eq!(caret_stop(&draw_frame(&app, 120, 12)), Some("enabled"));
    assert!(
        app.services.detail_max_scroll.get() > 0,
        "the card overflows 120x12"
    );

    // `↓` on the only stop scrolls one line at a time; once the enabled row
    // scrolls out the top, focus is none (no stop in view).
    let max = app.services.detail_max_scroll.get();
    let mut saw_none = false;
    for _ in 0..max {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        if caret_stop(&draw_frame(&app, 120, 12)).is_none() {
            saw_none = true;
            break;
        }
    }
    assert!(saw_none, "focus leaves once the enabled row scrolls out");
    assert_eq!(app.services.shunt_focus_kind(), None);
    let hints = super::super::footer::services_hints(&app);
    assert!(
        hints.iter().all(|(k, _)| *k != "space/↵" && *k != "f"),
        "no verb offered with no stop in view: {hints:?}"
    );

    // The next frame with a stop in view (a taller pane) re-settles focus to
    // it — no key press.
    assert_eq!(caret_stop(&draw_frame(&app, 120, 16)), Some("enabled"));
}

/// A shunt card with no actionable line (the absent card with an unread bind)
/// still scrolls one rendered line per ↑↓, and the footer's `↑↓ scroll` is
/// then true (round 1 S14's behaviour, restored).
#[test]
fn a_stopless_card_still_scrolls_one_line_per_press() {
    use crate::tui::app::{ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Absent);
    slot.reason = Some(
        (0..10)
            .map(|i| format!("parse error line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let standalone = crate::tui::app::StandaloneShunt {
        found: Some(std::path::PathBuf::from("/cfg/shunt.yaml")),
        unread_bind: Some(crate::tui::app::UnreadBind::Yaml),
        unread_config: Some(crate::tui::app::UnreadConfig::Yaml),
        unread_env: false,
        answer: None,
    };
    let card = crate::tui::app::shunt_card(
        &slot,
        false,
        false,
        Some(&standalone),
        None,
        None,
        None,
        None,
        false,
        false,
        false,
    );
    assert!(card.shunt_focus.is_empty(), "fixture: no actionable line");
    let mut app = app_with(card);

    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(app.services.focus, ServicesFocus::Detail);
    let max = app.services.detail_max_scroll.get() as usize;
    assert!(max > 0, "the stopless card overflows 80x14");

    // The footer names ↑↓ scroll exactly while a press would scroll.
    let hints = super::super::footer::services_hints(&app);
    assert!(
        hints.iter().any(|(k, v)| *k == "↑↓" && *v == "scroll"),
        "the footer says ↑↓ scroll: {hints:?}"
    );

    // One line per press, clamped at either end.
    for _ in 0..max {
        let before = app.services.shunt_scroll.get();
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        assert_eq!(
            app.services.shunt_scroll.get(),
            before + 1,
            "one line per press"
        );
    }
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(app.services.shunt_scroll.get(), max, "clamped at the end");
    handle_key(&mut app, crate::testutil::key(KeyCode::Up));
    assert_eq!(
        app.services.shunt_scroll.get(),
        max - 1,
        "↑ scrolls back one line"
    );
}

/// Keys and commands inside prose in a detail pane: the plugin
/// row's unprobed `claude` line names `r` ACCENT + bold, and the herdr row's
/// install sub-line names `clauth herdr install` ACCENT, never bold, both
/// drawn through the detail pane (the herdr line off `herdr_check` itself, the
/// plugin line off the fixture mirroring `plugin_check`'s).
#[test]
fn service_detail_lines_style_their_key_and_command() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let (_, buf) = render(&app_with(plugin_check_with_problems()));
    crate::testutil::assert_prose_part(&buf, "press r to probe", " r ", true);

    let not_installed = herdr_check(&probe(Some("0.8.0"), None, None), Some(&healthy_config()));
    let (_, buf) = render(&app_with(not_installed));
    crate::testutil::assert_prose_part(&buf, "clauth herdr install", "clauth herdr install", false);
}

/// A herdr probe error naming a command in backticks (the shared CLI form)
/// renders that command ACCENT, never bold, backticks gone, on the herdr card.
#[test]
fn a_herdr_probe_error_styles_its_backticked_command() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let failed = herdr_check(
        &probe(
            Some("0.8.0"),
            None,
            Some("`/opt/bin/herdr plugin list --json` failed"),
        ),
        Some(&healthy_config()),
    );
    let (_, buf) = render(&app_with(failed));
    crate::testutil::assert_prose_part(
        &buf,
        "/opt/bin/herdr plugin list --json failed",
        "/opt/bin/herdr plugin list --json",
        false,
    );
}

/// A generic detail (any label other than herdr/plugin/shunt) pages its scroll
/// offset one page — the arm the selectors and forms don't reach. The literal
/// is the detail pane's visible rows minus one at 80x14.
#[test]
fn services_generic_detail_pages_by_hand_pinned_offset() {
    use crate::tui::app::{Check, Health, ServicesFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let check = Check {
        label: "future",
        health: Health::Idle,
        detail: (0..30).map(|i| format!("detail line {i:02}")).collect(),
        fix: None,
        problems: Vec::new(),
        shunt_focus: Vec::new(),
        shunt_action_start: None,
    };
    let mut app = app_with(check);
    app.services.focus = ServicesFocus::Detail;
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    assert_eq!(
        app.services.detail_scroll, 7,
        "generic detail pages by viewport - 1"
    );
}

// ── shunt config section render pins ─────────────────────────────────────────

use crate::gateway_edit::{
    AccountView, AuthForm, AuthView, BindOverride, ConfigView, EnvFileFailure, ModelView,
    ProviderForm, RouteView, ServerView, UpstreamView, Written,
};
use crate::tui::app::{ShuntConfigRead, ShuntEntryKey};

/// A `Written` holding a plain string value (its TOML text irrelevant to the
/// render, which reads `value` first).
fn w(v: &str) -> Written {
    Written {
        raw: v.to_string(),
        value: Some(v.to_string()),
    }
}

/// A `${…}` reference value, as the engine spells it: `value` is the string
/// content, `raw` the quoted file text.
fn wref(v: &str) -> Written {
    Written {
        raw: format!("\"{v}\""),
        value: Some(v.to_string()),
    }
}

/// A shorthand-auth upstream (`auth = "<mode>"`), no other field set.
fn shorthand_up(name: &str, mode: &str) -> UpstreamView {
    UpstreamView {
        name: Some(w(name)),
        provider: None,
        kind: None,
        base_url: None,
        auth: Some(AuthView {
            form: AuthForm::Shorthand,
            mode: Some(w(mode)),
            account: None,
            accounts: None,
            env: None,
            header: None,
        }),
        effort: None,
        service_tier: None,
    }
}

/// A shorthand-auth upstream with no `name` key (a shape shunt itself would
/// refuse, but `read_view` reads): keyed by the empty name plus its occurrence.
fn nameless_up(mode: &str) -> UpstreamView {
    UpstreamView {
        name: None,
        ..shorthand_up("unused", mode)
    }
}

/// A `ConfigView` with a set bind + default provider and no entries.
fn base_view() -> ConfigView {
    ConfigView {
        server: ServerView {
            bind: Some(w("127.0.0.1:3001")),
            bind_override: BindOverride::None,
            default_provider: Some(w("anthropic")),
        },
        form: ProviderForm::Upstreams,
        upstreams: Vec::new(),
        models: Vec::new(),
        routes: Vec::new(),
    }
}

/// An app whose shunt card is the full slot plus `view` as its config section.
fn config_view_app(view: ConfigView, expanded: std::collections::HashSet<ShuntEntryKey>) -> App {
    config_view_app_with_tokens(view, expanded, crate::tui::app::ShuntTokens::default())
}

/// [`config_view_app`] whose `CLIENT TOKENS` group holds `tokens`, its stops
/// built from the same tokens the render reads.
fn config_view_app_with_tokens(
    view: ConfigView,
    expanded: std::collections::HashSet<ShuntEntryKey>,
    tokens: crate::tui::app::ShuntTokens,
) -> App {
    let mut card = crate::tui::app::shunt_check(&shunt_slot_full(), true, None);
    let drafts = crate::tui::app::ShuntDrafts::default();
    card.shunt_focus.extend(crate::tui::app::config_field_stops(
        &view, &expanded, &drafts, &tokens,
    ));
    let mut app = app_with(card);
    app.services.shunt_config = Some(ShuntConfigRead::Ok(Box::new(view)));
    app.services.shunt_expanded = expanded;
    app.services.shunt_drafts = drafts;
    app.services.shunt_tokens = tokens;
    app
}

/// One drawn frame of the shunt card as the walk sees it: the detail pane's
/// visible rows, the caret's index among them, the caret's row in the whole
/// card, and the card's height.
struct ShuntWalkFrame {
    pane: Vec<String>,
    caret: usize,
    row: usize,
    total: usize,
}

/// [`shunt_walk_frame`], or `None` while no stop holds the caret (a `↓` over
/// card text with no stop in view scrolls one line and focuses nothing).
fn shunt_walk_frame_focused(app: &App, w: u16, h: u16) -> Option<ShuntWalkFrame> {
    // The draw re-settles the held focus first, so read it after one.
    let _ = draw_frame(app, w, h);
    app.services.shunt_focus_kind()?;
    Some(shunt_walk_frame(app, w, h))
}

/// Draw `app` at `w`×`h` and read the walk's frame. The card must overflow
/// its viewport, or the total read off the scroll bound means nothing.
fn shunt_walk_frame(app: &App, w: u16, h: u16) -> ShuntWalkFrame {
    let rows = draw_frame(app, w, h);
    let pane: Vec<String> = rows
        .iter()
        .filter_map(|r| r.split("││").nth(1))
        .map(|r| r.trim_matches('│').to_string())
        .collect();
    let viewport = usize::from(app.services.shunt_viewport.get());
    let max = usize::from(app.services.detail_max_scroll.get());
    assert!(max > 0, "the card fits its viewport:\n{}", rows.join("\n"));
    assert_eq!(pane.len(), viewport, "pane rows:\n{}", rows.join("\n"));
    let caret = pane
        .iter()
        .position(|r| r.contains('❯'))
        .unwrap_or_else(|| panic!("no caret in the detail pane:\n{}", rows.join("\n")));
    let scroll = app.services.shunt_scroll.get().min(max);
    ShuntWalkFrame {
        row: scroll + caret,
        total: max + viewport,
        pane,
        caret,
    }
}

/// `n` collapsed `api_key` upstreams, `u00` on: single-line entry stops.
fn collapsed_upstreams(n: usize) -> Vec<UpstreamView> {
    (0..n)
        .map(|i| shorthand_up(&format!("u{i:02}"), "api_key"))
        .collect()
}

/// Walked by `↓` over a card taller than its viewport, every focused
/// single-line stop keeps `SCROLL_PAD` (2) visible rows below it while the
/// card has that many rows left below it; walked back by `↑`, two rows above
/// it while that many rows sit above it.
#[test]
fn the_shunt_walk_keeps_two_rows_past_its_focus_while_content_remains() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = collapsed_upstreams(14);
    let mut app = config_view_app(view, std::collections::HashSet::new());
    let _ = draw_frame(&app, 100, 16);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    // The config section alone holds 20 stops (bind, default_provider, 14
    // entries, four `+ add` rows); each walk lands on every one of them.
    let mut focused = 0;
    for press in 0..60 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let Some(f) = shunt_walk_frame_focused(&app, 100, 16) else {
            continue;
        };
        focused += 1;
        let below = f.pane.len() - 1 - f.caret;
        let left = f.total - 1 - f.row;
        assert!(
            below >= left.min(2),
            "↓ #{press}: {below} rows below the focus, {left} left in the card:\n{}",
            f.pane.join("\n")
        );
    }
    assert!(focused >= 20, "the ↓ walk focused {focused} frames");
    let f = shunt_walk_frame(&app, 100, 16);
    assert!(
        f.pane[f.caret].contains("+ add token"),
        "the ↓ walk ends on the card's last stop:\n{}",
        f.pane.join("\n")
    );
    let mut focused = 0;
    for press in 0..60 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Up));
        let Some(f) = shunt_walk_frame_focused(&app, 100, 16) else {
            continue;
        };
        focused += 1;
        assert!(
            f.caret >= f.row.min(2),
            "↑ #{press}: {} rows above the focus, {} above it in the card:\n{}",
            f.caret,
            f.row,
            f.pane.join("\n")
        );
    }
    assert!(focused >= 20, "the ↑ walk focused {focused} frames");
}

/// A multi-line stop counts from its LAST row: `↓` landing on an `auth mode`
/// cycle row that wraps onto continuation lines shows the whole block and two
/// rows past it (`accounts`, `effort`), never just its first line at the edge.
#[test]
fn the_shunt_walk_keeps_two_rows_past_a_wrapped_stops_last_line() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    let mut upstreams = vec![UpstreamView {
        name: Some(w("anthropic")),
        provider: None,
        kind: None,
        base_url: None,
        auth: Some(AuthView {
            form: AuthForm::Inline,
            mode: Some(w("claude_oauth")),
            account: None,
            accounts: Some(vec![
                AccountView::Name(w("alice")),
                AccountView::Name(w("bob")),
            ]),
            env: None,
            header: None,
        }),
        effort: None,
        service_tier: None,
    }];
    upstreams.extend(collapsed_upstreams(6));
    view.upstreams = upstreams;
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "anthropic".into(),
        occurrence: 0,
    }]);
    let mut app = config_view_app(view, expanded);
    let _ = draw_frame(&app, 100, 16);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let mut landed = None;
    for _ in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let Some(f) = shunt_walk_frame_focused(&app, 100, 16) else {
            continue;
        };
        if f.pane[f.caret].contains("auth mode") {
            landed = Some(f);
            break;
        }
    }
    let f = landed.expect("the ↓ walk reaches the auth mode row");
    let next = f.pane[f.caret..]
        .iter()
        .position(|r| r.trim_start().starts_with("accounts"))
        .map(|i| f.caret + i)
        .unwrap_or_else(|| {
            panic!(
                "the auth mode block's last line is off-screen:\n{}",
                f.pane.join("\n")
            )
        });
    let last = next - 1;
    assert!(
        f.pane[last]
            .trim_end_matches(['┊', '┃', ' '])
            .ends_with("none"),
        "the block ends on its last option:\n{}",
        f.pane.join("\n")
    );
    assert!(
        f.pane.len() - 1 - last >= 2,
        "{} rows below the block's last line:\n{}",
        f.pane.len() - 1 - last,
        f.pane.join("\n")
    );
}

/// A second multi-line stop kind: the open upstream draft's `save upstream`
/// row with a refusal `└` line trailing it. `↓` landing on it shows the whole
/// block (the save row + the `└` line) and keeps `SCROLL_PAD` rows past the `└`
/// line while the card has them — never just the save row at the edge.
#[test]
fn the_shunt_walk_keeps_two_rows_past_a_save_refusals_last_line() {
    use crate::tui::app::{
        ShuntDrafts, ShuntTokens, UpstreamDraft, config_field_stops, handle_key, shunt_check,
    };
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = collapsed_upstreams(8);
    let expanded = std::collections::HashSet::new();
    let drafts = ShuntDrafts {
        upstream: Some(UpstreamDraft {
            refusal: Some("save refused: name is required".to_string()),
            ..UpstreamDraft::default()
        }),
        ..ShuntDrafts::default()
    };
    let mut card = shunt_check(&shunt_slot_full(), true, None);
    card.shunt_focus.extend(config_field_stops(
        &view,
        &expanded,
        &drafts,
        &ShuntTokens::default(),
    ));
    let mut app = app_with(card);
    app.services.shunt_config = Some(ShuntConfigRead::Ok(Box::new(view)));
    app.services.shunt_expanded = expanded;
    app.services.shunt_drafts = drafts;
    app.services.shunt_tokens = ShuntTokens::default();
    let _ = draw_frame(&app, 100, 16);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let mut landed = None;
    for _ in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let Some(f) = shunt_walk_frame_focused(&app, 100, 16) else {
            continue;
        };
        if f.pane[f.caret].contains("save upstream") {
            landed = Some(f);
            break;
        }
    }
    let f = landed.expect("the ↓ walk reaches the save upstream row");
    let last = f.pane[f.caret..]
        .iter()
        .position(|r| r.trim_start().starts_with('└'))
        .map(|i| f.caret + i)
        .unwrap_or_else(|| {
            panic!(
                "the save row's refusal line is off-screen:\n{}",
                f.pane.join("\n")
            )
        });
    let below = f.pane.len() - 1 - last;
    let left = f.total - 1 - (f.row + last - f.caret);
    assert!(
        below >= left.min(2),
        "{below} rows below the refusal, {left} left in the card:\n{}",
        f.pane.join("\n")
    );
}

/// `f` scrolling a fix line below the fold into view keeps two rows past it
/// while the card has them.
#[test]
fn the_shunt_f_keeps_two_rows_past_the_fix_it_scrolls_to() {
    use crate::tui::app::{Check, Health, ShuntFocus, ShuntFocusLine, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut detail: Vec<String> = (0..30).map(|i| format!("plan row {i:02}")).collect();
    detail[5] = "f  move stores in".to_string();
    detail[25] = "f  add admin key".to_string();
    let check = Check {
        label: "shunt",
        health: Health::Warn,
        detail,
        fix: Some(ServiceFix::MoveStoresIn),
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 5,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Fix {
                line: 25,
                fix: ServiceFix::AddAdminKey,
            },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let f = shunt_walk_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Admin)
    );
    assert!(
        f.pane[f.caret].contains("f  add admin key"),
        "the caret sits on the fix:\n{}",
        f.pane.join("\n")
    );
    let below = f.pane.len() - 1 - f.caret;
    assert!(
        below >= 2,
        "{below} rows below the fix, 4 left in the card:\n{}",
        f.pane.join("\n")
    );
}

/// From the two-fix card's dropped state (a `↓` text scroll dropped the move
/// fix, focus none), `f` lands on the admin fix — the next fix the render's
/// follow would not pull the view back for — with the offset never decreasing,
/// then repeated `f` walks the fixes in order (B1a).
#[test]
fn f_from_a_dropped_focus_lands_on_the_next_fix_without_pulling_the_view_back() {
    use crate::tui::app::{ShuntFocus, ShuntFocusLine, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut detail: Vec<String> = (0..30).map(|i| format!("plan row {i:02}")).collect();
    detail[5] = "f  move stores in".to_string();
    detail[25] = "f  add admin key".to_string();
    let check = Check {
        label: "shunt",
        health: Health::Warn,
        detail,
        fix: Some(ServiceFix::MoveStoresIn),
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 5,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Fix {
                line: 25,
                fix: ServiceFix::AddAdminKey,
            },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    // `↓` until the caret drops (the move fix left the padded band).
    for _ in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        if shunt_walk_frame_focused(&app, 80, 14).is_none() {
            break;
        }
    }
    assert_eq!(
        app.services.shunt_focus_kind(),
        None,
        "the ↓ walk drops the move fix"
    );
    let max = app.services.detail_max_scroll.get() as usize;
    let dropped = app.services.shunt_scroll.get().min(max);
    // `f` from the dropped state lands on the admin fix, and the view never
    // moves up against the key.
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Admin),
        "f from the dropped state lands on the admin fix"
    );
    let after = app.services.shunt_scroll.get().min(max);
    assert!(
        after >= dropped,
        "f moves the view up: {dropped} -> {after}"
    );
    // Repeated `f` walks the fixes in order (admin -> move -> admin).
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Move),
        "the next f wraps to the move fix"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    let _ = draw_frame(&app, 80, 14);
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Admin),
        "the next f walks back to the admin fix"
    );
}

/// `↓` on the two-fix card (fixes at lines 5 and 25) scrolls monotonically
/// down — never re-taking the stop a text scroll dropped and pulling the view
/// back against the key — until it lands on the admin fix (B1).
#[test]
fn down_walks_the_two_fix_card_monotonically_to_admin() {
    use crate::tui::app::{ShuntFocus, ShuntFocusLine, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut detail: Vec<String> = (0..30).map(|i| format!("plan row {i:02}")).collect();
    detail[5] = "f  move stores in".to_string();
    detail[25] = "f  add admin key".to_string();
    let check = Check {
        label: "shunt",
        health: Health::Warn,
        detail,
        fix: Some(ServiceFix::MoveStoresIn),
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 5,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Fix {
                line: 25,
                fix: ServiceFix::AddAdminKey,
            },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let max = app.services.detail_max_scroll.get() as usize;
    let mut prev = app.services.shunt_scroll.get().min(max);
    let mut reached_admin = false;
    for press in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let _ = draw_frame(&app, 80, 14);
        let scroll = app.services.shunt_scroll.get().min(max);
        assert!(
            scroll >= prev,
            "↓ #{press} moves the view up: {prev} -> {scroll}"
        );
        prev = scroll;
        if app.services.focused_shunt_focus().map(|s| s.kind()) == Some(ShuntFocusLine::Admin) {
            reached_admin = true;
            break;
        }
    }
    assert!(reached_admin, "the ↓ walk reaches the admin fix");
}

/// The `↑` mirror of the two-fix walk: from the admin fix, `↑` scrolls
/// monotonically up — never re-taking the dropped stop and pulling the view
/// back down against the key — until it lands on the move fix (B1).
#[test]
fn up_walks_back_from_admin_monotonically_to_move() {
    use crate::tui::app::{ShuntFocus, ShuntFocusLine, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut detail: Vec<String> = (0..30).map(|i| format!("plan row {i:02}")).collect();
    detail[5] = "f  move stores in".to_string();
    detail[25] = "f  add admin key".to_string();
    let check = Check {
        label: "shunt",
        health: Health::Warn,
        detail,
        fix: Some(ServiceFix::MoveStoresIn),
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 5,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Fix {
                line: 25,
                fix: ServiceFix::AddAdminKey,
            },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    // Walk ↓ onto the admin fix first.
    for _ in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let _ = draw_frame(&app, 80, 14);
        if app.services.focused_shunt_focus().map(|s| s.kind()) == Some(ShuntFocusLine::Admin) {
            break;
        }
    }
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(ShuntFocusLine::Admin),
        "the ↓ walk reached the admin fix"
    );
    let max = app.services.detail_max_scroll.get() as usize;
    let mut prev = app.services.shunt_scroll.get().min(max);
    let mut reached_move = false;
    for press in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Up));
        let _ = draw_frame(&app, 80, 14);
        let scroll = app.services.shunt_scroll.get().min(max);
        assert!(
            scroll <= prev,
            "↑ #{press} moves the view down: {prev} -> {scroll}"
        );
        prev = scroll;
        if app.services.focused_shunt_focus().map(|s| s.kind()) == Some(ShuntFocusLine::Move) {
            reached_move = true;
            break;
        }
    }
    assert!(reached_move, "the ↑ walk reaches the move fix");
}

/// `↓` on the refused plan card at 60x14 scrolls monotonically down — no
/// press ever moves the view up — until it reaches the card's end, so the
/// refusal's last lines are reachable (B1).
#[test]
fn down_scrolls_the_refused_card_at_60x14_monotonically_to_the_end() {
    use crate::tui::app::{MovePlanOutcome, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    slot.version = Some("0.49.1".to_string());
    slot.reason = Some("port busy".to_string());
    slot.pid = Some(4242);
    slot.port = Some(3001);
    slot.restarts = 2;
    slot.last_exit = Some(crate::daemon::gateway::ExitReport {
        code: Some(1),
        signal: None,
    });
    let refusal = "the standalone's home names no absolute directory, so clauth cannot tell where the codex CLI's own login (~/.codex/auth.json) sits; nothing was moved; set HOME to an absolute path in the env file, then run the move again".to_string();
    let mut app = shunt_card_app(
        slot,
        true,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Refused(refusal)),
        None,
    );
    let _ = draw_frame(&app, 60, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let max = app.services.detail_max_scroll.get() as usize;
    assert!(max > 0, "the refused card overflows 60x14");
    let mut prev = app.services.shunt_scroll.get().min(max);
    let mut reached = false;
    for press in 0..80 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let _ = draw_frame(&app, 60, 14);
        let scroll = app.services.shunt_scroll.get().min(max);
        assert!(
            scroll >= prev,
            "↓ #{press} moves the view up: {prev} -> {scroll}"
        );
        prev = scroll;
        if scroll == max {
            reached = true;
            break;
        }
    }
    assert!(
        reached,
        "↓ reaches the end of the refused card (scroll {prev} of {max})"
    );
}

/// The `↑` mirror on the refused plan card at 60x14: from the end, `↑` scrolls
/// monotonically up — no press ever moves the view down — back to the top (B1).
#[test]
fn up_scrolls_the_refused_card_at_60x14_monotonically_to_the_top() {
    use crate::tui::app::{MovePlanOutcome, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    slot.version = Some("0.49.1".to_string());
    slot.reason = Some("port busy".to_string());
    slot.pid = Some(4242);
    slot.port = Some(3001);
    slot.restarts = 2;
    slot.last_exit = Some(crate::daemon::gateway::ExitReport {
        code: Some(1),
        signal: None,
    });
    let refusal = "the standalone's home names no absolute directory, so clauth cannot tell where the codex CLI's own login (~/.codex/auth.json) sits; nothing was moved; set HOME to an absolute path in the env file, then run the move again".to_string();
    let mut app = shunt_card_app(
        slot,
        true,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Refused(refusal)),
        None,
    );
    let _ = draw_frame(&app, 60, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let max = app.services.detail_max_scroll.get() as usize;
    assert!(max > 0, "the refused card overflows 60x14");
    // Walk ↓ to the end first.
    for _ in 0..80 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let _ = draw_frame(&app, 60, 14);
        if app.services.shunt_scroll.get().min(max) == max {
            break;
        }
    }
    assert_eq!(
        app.services.shunt_scroll.get().min(max),
        max,
        "the ↓ walk reached the end"
    );
    let mut prev = app.services.shunt_scroll.get().min(max);
    let mut reached = false;
    for press in 0..80 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Up));
        let _ = draw_frame(&app, 60, 14);
        let scroll = app.services.shunt_scroll.get().min(max);
        assert!(
            scroll <= prev,
            "↑ #{press} moves the view down: {prev} -> {scroll}"
        );
        prev = scroll;
        if scroll == 0 {
            reached = true;
            break;
        }
    }
    assert!(
        reached,
        "↑ reaches the top of the refused card (scroll {prev})"
    );
}

/// A refusal raised on the focused `save upstream` row after `↵` (the empty
/// draft refuses with `set its name first`) grows the block on its own event,
/// and the render's per-draw follow keeps `SCROLL_PAD` rows past the new last
/// line — the addendum's own case (F1).
#[test]
fn the_shunt_render_follows_a_refusal_raised_on_the_focused_save_row() {
    use crate::tui::app::{
        ShuntDrafts, ShuntTokens, UpstreamDraft, config_field_stops, handle_key, shunt_check,
    };
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = collapsed_upstreams(8);
    let expanded = std::collections::HashSet::new();
    let drafts = ShuntDrafts {
        upstream: Some(UpstreamDraft::default()),
        ..ShuntDrafts::default()
    };
    let mut card = shunt_check(&shunt_slot_full(), true, None);
    card.shunt_focus.extend(config_field_stops(
        &view,
        &expanded,
        &drafts,
        &ShuntTokens::default(),
    ));
    let mut app = app_with(card);
    app.services.shunt_config = Some(ShuntConfigRead::Ok(Box::new(view)));
    app.services.shunt_expanded = expanded;
    app.services.shunt_drafts = drafts;
    app.services.shunt_tokens = ShuntTokens::default();
    let _ = draw_frame(&app, 60, 16);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let mut landed = None;
    for _ in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let Some(f) = shunt_walk_frame_focused(&app, 60, 16) else {
            continue;
        };
        if f.pane[f.caret].contains("save upstream") {
            landed = Some(f);
            break;
        }
    }
    let _ = landed.expect("the ↓ walk reaches the save upstream row");
    // `↵` raises the refusal; the render re-follows the grown block.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let f = shunt_walk_frame(&app, 60, 16);
    let last = f.pane[f.caret..]
        .iter()
        .position(|r| r.trim_start().starts_with('└'))
        .map(|i| f.caret + i)
        .unwrap_or_else(|| {
            panic!(
                "the save row's refusal line is off-screen:\n{}",
                f.pane.join("\n")
            )
        });
    let below = f.pane.len() - 1 - last;
    let left = f.total - 1 - (f.row + last - f.caret);
    assert!(
        below >= left.min(2),
        "{below} rows below the refusal, {left} left in the card:\n{}",
        f.pane.join("\n")
    );
}

/// `PageDown` moves the offset by exactly `viewport - 1` lines and settles on
/// the first stop already inside the padded band — the line-10 admin fix at
/// scroll 7 (pane row 3, two rows of pad above it), never a shorter page that
/// lands on a stop outside the band and lets the render's follow pull it back
/// (F2, exact landing).
#[test]
fn the_shunt_page_keys_keep_two_rows_past_their_focus() {
    use crate::tui::app::{Check, Health, ServiceFix, ShuntFocus, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let check = Check {
        label: "shunt",
        health: Health::Warn,
        detail: (0..30).map(|i| format!("plan row {i:02}")).collect(),
        fix: Some(ServiceFix::MoveStoresIn),
        problems: Vec::new(),
        shunt_focus: vec![
            ShuntFocus::Fix {
                line: 7,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Fix {
                line: 10,
                fix: ServiceFix::AddAdminKey,
            },
            ShuntFocus::Fix {
                line: 14,
                fix: ServiceFix::MoveStoresIn,
            },
            ShuntFocus::Fix {
                line: 21,
                fix: ServiceFix::AddAdminKey,
            },
        ],
        shunt_action_start: Some(0),
    };
    let mut app = app_with(check);
    let _ = draw_frame(&app, 80, 14);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    handle_key(&mut app, crate::testutil::key(KeyCode::PageDown));
    let f = shunt_walk_frame(&app, 80, 14);
    assert_eq!(
        app.services.shunt_scroll.get(),
        7,
        "the page moves the offset by viewport - 1 = 7 lines:\n{}",
        f.pane.join("\n")
    );
    assert_eq!(
        app.services.focused_shunt_focus().map(|s| s.kind()),
        Some(crate::tui::app::ShuntFocusLine::Admin),
        "the page settles on the line-10 admin fix"
    );
    assert_eq!(
        f.row,
        10,
        "the caret sits on the line-10 fix, two rows of pad above it:\n{}",
        f.pane.join("\n")
    );
}

/// Scrolling card text with `↓` drops the caret once the focused row comes
/// within two rows of the top (the visible change): each press before that
/// scrolls one line and keeps the caret, and the render never pulls the scroll
/// back once the caret is gone.
#[test]
fn a_text_scroll_drops_the_caret_once_the_focused_row_nears_the_top() {
    use crate::tui::app::{MovePlanOutcome, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(super::theme::Tier::Full);
    let record = shunt_record(&home, true);
    let mut slot = shunt_slot(crate::daemon::gateway::GatewayState::Disabled);
    slot.config = Some(record.config().display().to_string());
    slot.binary = Some("shunt".to_string());
    slot.version = Some("0.49.1".to_string());
    slot.reason = Some("port busy".to_string());
    slot.pid = Some(4242);
    slot.port = Some(3001);
    slot.restarts = 2;
    slot.last_exit = Some(crate::daemon::gateway::ExitReport {
        code: Some(1),
        signal: None,
    });
    let refusal = "the standalone's home names no absolute directory, so clauth cannot tell where the codex CLI's own login (~/.codex/auth.json) sits; nothing was moved; set HOME to an absolute path in the env file, then run the move again".to_string();
    let mut app = shunt_card_app(
        slot,
        true,
        Some(record),
        Some(crate::gateway::AdminNeed::WriteKey),
        Some(MovePlanOutcome::Refused(refusal)),
        None,
    );
    let _ = draw_frame(&app, 80, 16);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    // Walk onto the admin fix; at 80x16 its block sits just above the fold so
    // the scroll range below is long enough to drive its row into the top band.
    let mut on_admin = false;
    for _ in 0..6 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        if caret_stop(&draw_frame(&app, 80, 16)) == Some("admin") {
            on_admin = true;
            break;
        }
    }
    assert!(on_admin, "the ↓ walk reaches the admin fix");

    let mut dropped = false;
    let mut press = 0;
    while press < 60 {
        let Some(f_before) = shunt_walk_frame_focused(&app, 80, 16) else {
            dropped = true;
            break;
        };
        let scroll_before = f_before.row - f_before.caret;
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        press += 1;
        match shunt_walk_frame_focused(&app, 80, 16) {
            Some(f_after) => {
                let scroll_after = f_after.row - f_after.caret;
                assert_eq!(scroll_after, scroll_before + 1, "one line per press");
            }
            None => {
                dropped = true;
                break;
            }
        }
    }
    assert!(dropped, "the caret drops once the admin row nears the top");
}

/// The walk moves the view only to restore the pad: a landing whose stop is
/// already inside the padded band leaves the scroll put, and one that is not
/// moves by exactly the rows the follow needs (the block's last line ends two
/// rows from the bottom, or fewer at the card's end) — never re-pinned to
/// `row - 2` (F4).
#[test]
fn the_shunt_walk_moves_the_view_only_to_restore_the_pad() {
    use crate::tui::app::handle_key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = collapsed_upstreams(14);
    let mut app = config_view_app(view, std::collections::HashSet::new());
    let _ = draw_frame(&app, 100, 16);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let mut prev_scroll = 0usize;
    let mut focused = 0;
    for press in 0..40 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let Some(f) = shunt_walk_frame_focused(&app, 100, 16) else {
            continue;
        };
        focused += 1;
        let scroll = f.row - f.caret;
        let below = f.pane.len() - 1 - f.caret;
        let left = f.total - 1 - f.row;
        assert!(
            scroll == prev_scroll || below == left.min(2),
            "↓ #{press}: scroll moved {prev_scroll} -> {scroll} with {below} below, {left} left:\n{}",
            f.pane.join("\n")
        );
        prev_scroll = scroll;
    }
    assert!(focused >= 20, "the ↓ walk focused {focused} frames");
}

/// A token row whose last-token refusal drops onto its own `└` line keeps two
/// rows past that line when `↓` lands on it — the token end site is bound (F5).
/// Several tokens sit below `alice`, so the full two-row pad applies and a
/// one-line-short block end would red this pin.
#[test]
fn the_shunt_walk_keeps_two_rows_past_a_token_rows_refusal_line() {
    use crate::tui::app::{ShuntTokens, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = collapsed_upstreams(8);
    let tokens = ShuntTokens {
        stored: vec![
            "alice".to_string(),
            "bob".to_string(),
            "carol".to_string(),
            "dave".to_string(),
        ],
        ..ShuntTokens::default()
    };
    let mut app = config_view_app_with_tokens(view, std::collections::HashSet::new(), tokens);
    let _ = draw_frame(&app, 100, 16);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let mut landed = None;
    for _ in 0..60 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let Some(f) = shunt_walk_frame_focused(&app, 100, 16) else {
            continue;
        };
        if f.pane[f.caret].contains("alice") {
            landed = Some(f);
            break;
        }
    }
    let _ = landed.expect("the ↓ walk reaches the token row");
    // The focus setter retires a token refusal the moment focus leaves its row,
    // so it is set only once the walk holds the token.
    app.services.shunt_token_refusal.replace(Some((
        "alice".to_string(),
        "cannot remove the last token".to_string(),
    )));
    let f = shunt_walk_frame(&app, 100, 16);
    let last = f.pane[f.caret..]
        .iter()
        .position(|r| r.trim_start().starts_with('└'))
        .map(|i| f.caret + i)
        .unwrap_or_else(|| {
            panic!(
                "the token row's refusal line is off-screen:\n{}",
                f.pane.join("\n")
            )
        });
    let below = f.pane.len() - 1 - last;
    let left = f.total - 1 - (f.row + last - f.caret);
    assert!(
        left >= 2,
        "{left} rows left below the refusal; the fixture needs at least two:\n{}",
        f.pane.join("\n")
    );
    assert!(
        below >= 2,
        "{below} rows below the refusal, {left} left in the card:\n{}",
        f.pane.join("\n")
    );
}

/// An expanded entry's `remove <kind>` row with a refusal `└` line keeps two
/// rows past that line when `↓` lands on it — the remove end site is bound
/// (F5).
#[test]
fn the_shunt_walk_keeps_two_rows_past_a_remove_refusals_last_line() {
    use crate::tui::app::{ShuntEntryKey, ShuntRemoveRefusal, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = collapsed_upstreams(8);
    let key = ShuntEntryKey::Upstream {
        name: "u00".into(),
        occurrence: 0,
    };
    let expanded = std::collections::HashSet::from([key.clone()]);
    let mut app = config_view_app(view.clone(), expanded);
    let _ = draw_frame(&app, 100, 16);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    let mut landed = None;
    for _ in 0..60 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        let Some(f) = shunt_walk_frame_focused(&app, 100, 16) else {
            continue;
        };
        if f.pane[f.caret].contains("remove upstream") {
            landed = Some(f);
            break;
        }
    }
    let _ = landed.expect("the ↓ walk reaches the remove upstream row");
    // The focus setter retires a remove refusal the moment focus leaves its
    // row, so it is set only once the walk holds the remove row.
    app.services
        .shunt_remove_refusal
        .replace(Some(ShuntRemoveRefusal {
            key,
            sentence: "cannot remove the only upstream".to_string(),
            view: Box::new(view),
        }));
    let f = shunt_walk_frame(&app, 100, 16);
    let last = f.pane[f.caret..]
        .iter()
        .position(|r| r.trim_start().starts_with('└'))
        .map(|i| f.caret + i)
        .unwrap_or_else(|| {
            panic!(
                "the remove row's refusal line is off-screen:\n{}",
                f.pane.join("\n")
            )
        });
    let below = f.pane.len() - 1 - last;
    let left = f.total - 1 - (f.row + last - f.caret);
    assert!(
        below >= left.min(2),
        "{below} rows below the refusal, {left} left in the card:\n{}",
        f.pane.join("\n")
    );
}

/// The detail pane's content rows keeping the card's own indentation: the
/// `││` split drops both panes' borders, then exactly the one-cell pane
/// padding is dropped from the left and the right padding is trimmed — the
/// card's leading gutter (the entry rows' caret slot, the expanded fields'
/// indent) stays, where [`detail_rows`]'s `.trim()` would strip it. Blank
/// rows stay (the one blank line before `SERVER` is pinned), trailing pane
/// padding trimmed.
fn config_detail_rows(rows: &[String]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .filter_map(|r| r.split("││").nth(1))
        .map(|r| {
            let r = r.trim_matches('│');
            let r = r.strip_prefix(' ').unwrap_or(r);
            r.trim_end().to_string()
        })
        .collect();
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

/// Render the config section over the full slot at a tall viewport (so the
/// config rows never overflow into a scrollbar) and pin the detail rows.
fn assert_config_detail(
    view: ConfigView,
    expanded: std::collections::HashSet<ShuntEntryKey>,
    expected: &[&str],
) {
    let app = config_view_app(view, expanded);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 40)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert_eq!(
        config_detail_rows(&rows),
        expected,
        "config detail rows:\n{}",
        rows.join("\n")
    );
}

/// Render the config section at terminal `width` and return its rows from the
/// `SERVER` eyebrow on, for the narrow-width `…` and duplicate-identity pins.
fn render_config_section(
    view: ConfigView,
    expanded: std::collections::HashSet<ShuntEntryKey>,
    width: u16,
) -> Vec<String> {
    let app = config_view_app(view, expanded);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 40)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let detail = config_detail_rows(&rows);
    let at = detail.iter().position(|r| r == "SERVER").expect("SERVER");
    detail[at..].to_vec()
}

/// The config-section detail rows of an app already carrying view + state (a
/// save in flight, a focus), so a saving row's spinner/`checking` is pinable.
fn render_app_config_rows(app: &App) -> Vec<String> {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app, &[])).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    config_detail_rows(&rows)
}

/// The groups follow the card's lines under `SERVER` / `UPSTREAMS` / `MODELS` /
/// `ROUTES` eyebrows, each upstream a `▶` row with its name padded to the key
/// column and a dim summary; an empty group still shows its eyebrow.
#[test]
fn the_shunt_config_renders_upstreams_collapsed() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![
        shorthand_up("anthropic", "claude_oauth"),
        shorthand_up("openai", "api_key"),
    ];
    assert_config_detail(
        view,
        std::collections::HashSet::new(),
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  ▶ anthropic       claude_oauth",
            "  ▶ openai          api_key",
            "  + add upstream",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// An expanded upstream lists its fields indented under it, in the brief's
/// order: a `claude_oauth` (pool OAuth) auth renders `accounts` alone — no
/// `account` (unset), no `auth env`/`auth header` — and absent keys read `—`.
#[test]
fn the_shunt_config_renders_one_upstream_expanded_with_accounts() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.server.default_provider = None;
    view.upstreams = vec![UpstreamView {
        name: Some(w("anthropic")),
        provider: None,
        kind: None,
        base_url: None,
        auth: Some(AuthView {
            form: AuthForm::Inline,
            mode: Some(w("claude_oauth")),
            account: None,
            accounts: Some(vec![
                AccountView::Name(w("alice")),
                AccountView::Name(w("bob")),
            ]),
            env: None,
            header: None,
        }),
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "anthropic".into(),
        occurrence: 0,
    }]);
    assert_config_detail(
        view,
        expanded,
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  —",
            "UPSTREAMS",
            "  ▼ anthropic       claude_oauth · 2 accounts",
            "    name            anthropic",
            "    provider        —",
            "    kind            —",
            "    base_url        —",
            "    auth mode       passthrough  api_key  chatgpt_oauth",
            "                    claude_oauth  xai_oauth  cursor_oauth",
            "                    kimi_oauth  google_oauth  antigravity_oauth",
            "                    none",
            "    accounts        alice, bob",
            "    effort          —",
            "    service_tier    —",
            "    remove upstream",
            "  + add upstream",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// A `[providers.*]` config renders its names as one `providers` row and the
/// convert-by-hand note under it; the model, route and server rows still render.
#[test]
fn the_shunt_config_renders_the_legacy_form() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.form = ProviderForm::Legacy {
        providers: vec!["anthropic".to_string(), "openai".to_string()],
    };
    assert_config_detail(
        view,
        std::collections::HashSet::new(),
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  providers         anthropic, openai",
            "  └ [providers.*] form; convert to [[upstreams]] by hand to edit",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// A legacy `[providers.*]` config offers no `+ add upstream` and no `remove
/// upstream` stop (its providers are read-only): the walk carries only the
/// model and route add rows.
#[test]
fn the_legacy_config_offers_no_upstream_add_or_remove() {
    use crate::tui::app::ShuntAddKind;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.form = ProviderForm::Legacy {
        providers: vec!["anthropic".to_string()],
    };
    let stops = crate::tui::app::config_field_stops(
        &view,
        &std::collections::HashSet::new(),
        &crate::tui::app::ShuntDrafts::default(),
        &crate::tui::app::ShuntTokens::default(),
    );
    assert!(
        !stops.iter().any(|s| matches!(
            s,
            crate::tui::app::ShuntFocus::Add {
                kind: ShuntAddKind::Upstream
            }
        )),
        "no `+ add upstream` stop on a legacy config"
    );
    assert!(
        !stops.iter().any(|s| matches!(
            s,
            crate::tui::app::ShuntFocus::Remove {
                key: crate::tui::app::ShuntEntryKey::Upstream { .. }
            }
        )),
        "no `remove upstream` stop on a legacy config"
    );
}
#[test]
fn the_shunt_config_renders_the_bind_override_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.server.bind_override = BindOverride::Set("SHUNT_SERVER__BIND".to_string());
    assert_config_detail(
        view,
        std::collections::HashSet::new(),
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001  set by SHUNT_SERVER__BIND",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  + add upstream",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// A `${…}` value renders as written, never resolved.
#[test]
fn the_shunt_config_renders_a_reference_value_as_written() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.routes = vec![RouteView {
        position: 1,
        model: Some(w("opus")),
        provider: Some(w("anthropic")),
        upstream_model: Some(wref("${MODEL}")),
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Route(1)]);
    assert_config_detail(
        view,
        expanded,
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  + add upstream",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  ▼ 1  opus → anthropic",
            "    model           opus",
            "    provider        anthropic",
            "    upstream_model  ${MODEL}",
            "    effort          —",
            "    service_tier    —",
            "    remove route",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// An absent key reads `—`, never shunt's default: a route with no model or
/// provider shows both sides faint in its label.
#[test]
fn the_shunt_config_renders_an_absent_key_as_em_dash() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.routes = vec![RouteView {
        position: 1,
        model: None,
        provider: None,
        upstream_model: None,
        effort: None,
        service_tier: None,
    }];
    assert_config_detail(
        view,
        std::collections::HashSet::new(),
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  + add upstream",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  ▶ 1  — → —",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// An expanded model lists `id`, `display_name`, then one row per
/// `upstream_model` entry labelled by its provider key; the collapsed summary
/// counts the upstreams.
#[test]
fn the_shunt_config_renders_a_model_with_two_upstreams_expanded() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.models = vec![ModelView {
        id: Some(w("opus")),
        display_name: Some(w("Opus")),
        upstream_model: Some(vec![
            ("anthropic".to_string(), w("claude-opus")),
            ("openai".to_string(), w("gpt-5")),
        ]),
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Model {
        id: "opus".into(),
        occurrence: 0,
    }]);
    assert_config_detail(
        view,
        expanded,
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  + add upstream",
            "MODELS",
            "  ▼ opus            2 upstreams",
            "    id              opus",
            "    display_name    Opus",
            "    anthropic       claude-opus",
            "    openai          gpt-5",
            "    remove model",
            "  + add model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// A collapsed route row labels `<position>  <model> → <provider>` with no
/// summary.
#[test]
fn the_shunt_config_renders_a_route_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.routes = vec![RouteView {
        position: 3,
        model: Some(w("opus")),
        provider: Some(w("anthropic")),
        upstream_model: None,
        effort: None,
        service_tier: None,
    }];
    assert_config_detail(
        view,
        std::collections::HashSet::new(),
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  + add upstream",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  ▶ 3  opus → anthropic",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

// ── C3 render pins: the add draft, save and remove rows ──────────────────────

use crate::tui::app::{ShuntAddKind, ShuntDrafts, ShuntSave, ShuntSaveKey, UpstreamDraft};

/// An app whose shunt card is the full slot plus `view`, `expanded` and
/// `drafts` (the open add forms).
fn config_draft_app(
    view: ConfigView,
    expanded: std::collections::HashSet<ShuntEntryKey>,
    drafts: ShuntDrafts,
) -> App {
    let mut card = crate::tui::app::shunt_check(&shunt_slot_full(), true, None);
    card.shunt_focus.extend(crate::tui::app::config_field_stops(
        &view,
        &expanded,
        &drafts,
        &crate::tui::app::ShuntTokens::default(),
    ));
    let mut app = app_with(card);
    app.services.shunt_config = Some(ShuntConfigRead::Ok(Box::new(view)));
    app.services.shunt_expanded = expanded;
    app.services.shunt_drafts = drafts;
    app
}

/// Render a config section over `view`/`expanded`/`drafts` and pin the detail
/// rows by equality (whole line vecs).
fn assert_config_detail_with(
    view: ConfigView,
    expanded: std::collections::HashSet<ShuntEntryKey>,
    drafts: ShuntDrafts,
    expected: &[&str],
) {
    let app = config_draft_app(view, expanded, drafts);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 40)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert_eq!(
        config_detail_rows(&rows),
        expected,
        "config detail rows:\n{}",
        rows.join("\n")
    );
}

/// An open upstream draft renders its blank field rows under the `+ add
/// upstream` row (Disclosure alignment, every unset value `—` faint) and ends
/// on the `save upstream` row.
#[test]
fn the_shunt_config_renders_an_open_upstream_draft() {
    let _home = crate::testutil::HomeSandbox::new();
    let view = base_view();
    let drafts = ShuntDrafts {
        upstream: Some(UpstreamDraft::default()),
        ..ShuntDrafts::default()
    };
    assert_config_detail_with(
        view,
        std::collections::HashSet::new(),
        drafts,
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  + add upstream",
            "    name            —",
            "    provider        —",
            "    kind            —",
            "    base_url        —",
            "    auth mode       —",
            "    effort          —",
            "    service_tier    —",
            "    save upstream",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// A model draft lists `id`, `display_name`, then one `upstream_model
/// <provider>` row per upstream the config declares, in config order.
#[test]
fn the_shunt_config_renders_a_model_draft_with_provider_rows() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![shorthand_up("a", "api_key"), shorthand_up("b", "api_key")];
    let drafts = ShuntDrafts {
        model: Some(crate::tui::app::ModelDraft {
            id: Some("opus".to_string()),
            ..crate::tui::app::ModelDraft::default()
        }),
        ..ShuntDrafts::default()
    };
    assert_config_detail_with(
        view,
        std::collections::HashSet::new(),
        drafts,
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  ▶ a               api_key",
            "  ▶ b               api_key",
            "  + add upstream",
            "MODELS",
            "  + add model",
            "    id              opus",
            "    display_name    —",
            "    a               —",
            "    b               —",
            "    save model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// A saving `save <kind>` row shows the spinner in its caret slot, its label
/// under the shimmer crest and a dim `checking` trail.
#[test]
fn the_shunt_config_renders_a_saving_save_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let view = base_view();
    let drafts = ShuntDrafts {
        upstream: Some(UpstreamDraft::default()),
        ..ShuntDrafts::default()
    };
    let mut app = config_draft_app(view, std::collections::HashSet::new(), drafts);
    app.tick_count = 0;
    app.services.shunt_save = Some(ShuntSave {
        key: ShuntSaveKey::SaveAdd(ShuntAddKind::Upstream),
        started: std::time::Instant::now(),
        value: None,
        entry: None,
    });
    let rows = render_app_config_rows(&app);
    let save_row = rows
        .iter()
        .find(|r| r.contains("save upstream"))
        .unwrap_or_else(|| panic!("no save row:\n{}", rows.join("\n")));
    assert_eq!(
        save_row, "⠋   save upstream  checking",
        "the saving save row shows spinner + shimmer + checking at the entry-child indent"
    );
}

/// A draft's refusal (a required field left blank, or the engine's sentence)
/// renders on the `└` DANGER line under the `save <kind>` row.
#[test]
fn the_shunt_config_renders_a_draft_refusal_under_the_save_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let view = base_view();
    let drafts = ShuntDrafts {
        upstream: Some(UpstreamDraft {
            refusal: Some("cannot add upstream: set its name first".to_string()),
            ..UpstreamDraft::default()
        }),
        ..ShuntDrafts::default()
    };
    assert_config_detail_with(
        view,
        std::collections::HashSet::new(),
        drafts,
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  + add upstream",
            "    name            —",
            "    provider        —",
            "    kind            —",
            "    base_url        —",
            "    auth mode       —",
            "    effort          —",
            "    service_tier    —",
            "    save upstream",
            " └ cannot add upstream: set its name first",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// A remove refusal renders on the `└` DANGER line directly under the entry's
/// `remove upstream` row, and under the RIGHT entry when two are expanded (the
/// entry still expanded).
#[test]
fn the_shunt_config_renders_a_remove_refusal_under_the_remove_row() {
    use crate::tui::app::ShuntRemoveRefusal;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![
        UpstreamView {
            name: Some(w("a")),
            provider: None,
            kind: None,
            base_url: None,
            auth: None,
            effort: None,
            service_tier: None,
        },
        UpstreamView {
            name: Some(w("b")),
            provider: None,
            kind: None,
            base_url: None,
            auth: None,
            effort: None,
            service_tier: None,
        },
    ];
    let entry_a = ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    };
    let entry_b = ShuntEntryKey::Upstream {
        name: "b".into(),
        occurrence: 0,
    };
    let expanded = std::collections::HashSet::from([entry_a, entry_b.clone()]);
    let app = config_draft_app(view, expanded, ShuntDrafts::default());
    let refusal_view = match &app.services.shunt_config {
        Some(ShuntConfigRead::Ok(view)) => view.clone(),
        _ => unreachable!("the app holds a read view"),
    };
    app.services.shunt_remove_refusal.replace(Some(ShuntRemoveRefusal {
        key: entry_b,
        sentence: "cannot remove upstream \"b\": a comment next to it belongs to no single entry; edit the config by hand".to_string(),
        view: refusal_view,
    }));
    let rows = render_app_config_rows(&app);
    // Pin the section from the first remove row: the `└` line sits directly
    // under `b`'s remove row, never `a`'s.
    let start = rows
        .iter()
        .position(|r| r == "    remove upstream")
        .expect("the first remove row");
    assert_eq!(
        &rows[start..],
        &[
            "    remove upstream",
            "  ▼ b",
            "    name            b",
            "    provider        —",
            "    kind            —",
            "    base_url        —",
            "    auth mode       —",
            "    effort          —",
            "    service_tier    —",
            "    remove upstream",
            " └ cannot remove upstream \"b\": a comment next to it belongs to no",
            "   single entry; edit the config by hand",
            "  + add upstream",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
        "the remove refusal renders under its own remove row:\n{}",
        rows.join("\n")
    );
}

// ── shunt config section key handling ────────────────────────────────────────

use crate::tui::app::handle_key;
use ratatui::crossterm::event::KeyCode;

/// Render the full frame at a tall viewport so every config entry is in view
/// (the key handler walks over the geometry this render publishes).
fn render_tall(app: &App) {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app, &[])).unwrap();
}

/// `↵` expands a focused entry and `space` collapses it, the disclosure
/// contract (both keys activate, independently per entry).
#[test]
fn a_config_entry_enter_expands_and_space_collapses() {
    use crate::tui::app::ShuntFocus;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![shorthand_up("anthropic", "claude_oauth")];
    let mut app = config_view_app(view, std::collections::HashSet::new());
    render_tall(&app);

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    // The field rows lead the walk: bind → default_provider → the entry.
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Entry {
                key: ShuntEntryKey::Upstream { name, .. }
            }) if name == "anthropic"
        ),
        "descend lands on bind, two steps reach the entry"
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        app.services
            .shunt_expanded
            .contains(&ShuntEntryKey::Upstream {
                name: "anthropic".into(),
                occurrence: 0
            }),
        "enter expands the entry"
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Char(' ')));
    assert!(
        !app.services
            .shunt_expanded
            .contains(&ShuntEntryKey::Upstream {
                name: "anthropic".into(),
                occurrence: 0
            }),
        "space collapses the entry"
    );
}

/// `↑↓` walks the card in order: the server field rows lead, then each entry
/// and its expanded field rows, in card order.
#[test]
fn the_config_focus_walk_reaches_fields_then_entries() {
    use crate::tui::app::{ShuntFieldKey, ShuntFocus, UpstreamFieldKind};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![
        shorthand_up("anthropic", "claude_oauth"),
        shorthand_up("openai", "api_key"),
    ];
    let mut app = config_view_app(
        view,
        std::collections::HashSet::from([ShuntEntryKey::Upstream {
            name: "anthropic".into(),
            occurrence: 0,
        }]),
    );
    render_tall(&app);

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Field {
                key: ShuntFieldKey::Bind
            })
        ),
        "descend lands on the bind field"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Field {
                key: ShuntFieldKey::DefaultProvider
            })
        ),
        "one step lands on default_provider"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Entry {
                key: ShuntEntryKey::Upstream { name, .. }
            }) if name == "anthropic"
        ),
        "two steps reach the first entry"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Field {
                key: ShuntFieldKey::Upstream { name, field: UpstreamFieldKind::Name, .. }
            }) if name == "anthropic"
        ),
        "the expanded entry's first field follows its entry row"
    );
}

/// A card with one action-section stop (`enabled`) in front of the config
/// stops, so `rebuild_shunt_config_stops`' truncate-to-action-prefix runs on a
/// real prefix (the action rows carry a detail `line`, the config stops none).
fn config_view_app_with_enabled(
    view: ConfigView,
    expanded: std::collections::HashSet<ShuntEntryKey>,
) -> App {
    let mut app = config_view_app(view, expanded);
    let check = app
        .services
        .checks
        .iter_mut()
        .find(|c| c.label == "shunt")
        .expect("the shunt check");
    check.shunt_action_start = Some(check.detail.len());
    check.detail.push(String::new());
    let line = check.detail.len();
    check.detail.push("enabled".to_string());
    check
        .shunt_focus
        .insert(0, crate::tui::app::ShuntFocus::Enabled { line });
    app
}

/// The focused caret row's text: the blurred row with its two-cell gutter caret
/// slot restored as `❯ `.
fn caret_over(own: &str) -> String {
    format!("❯ {}", own.get(2..).unwrap_or(""))
}

/// Walk every stop down to the end and back up, asserting at every step that
/// the caret row's text is that stop's own row text (the blurred render's row,
/// its caret slot restored) — the render and the walk agree on every stop, the
/// action-section prefix included.
fn assert_walk_lands_on_own_rows(app: &mut App) {
    use crate::tui::app::{ServicesFocus, ShuntFocus};
    app.services.focus = ServicesFocus::List;
    let rest = render_app_config_rows(app);
    let offset = rest
        .iter()
        .position(|r| r.starts_with("binary"))
        .expect("the binary row");
    let published = app.services.shunt_stop_rows.borrow().clone();
    let stops = app
        .services
        .selected_check()
        .map(|c| c.shunt_focus.clone())
        .expect("the shunt check");
    assert!(
        matches!(stops.first(), Some(ShuntFocus::Enabled { .. })),
        "the action-section stop stays in the walk after a toggle"
    );
    assert_eq!(published.len(), stops.len(), "one rendered row per stop");
    let own: Vec<String> = (0..stops.len())
        .map(|i| rest[offset + published[i]].clone())
        .collect();

    app.services.focus = ServicesFocus::Detail;
    app.services.set_shunt_focus(Some(stops[0].kind()));
    for (i, own_text) in own.iter().enumerate() {
        let focused = render_app_config_rows(app);
        let caret = focused
            .iter()
            .find(|r| r.contains('❯'))
            .unwrap_or_else(|| panic!("no caret row:\n{}", focused.join("\n")));
        assert_eq!(
            caret,
            &caret_over(own_text),
            "down stop {i} lands on its own row:\n{}",
            focused.join("\n")
        );
        if i + 1 < stops.len() {
            handle_key(app, crate::testutil::key(KeyCode::Down));
        }
    }
    for (i, own_text) in own.iter().enumerate().rev() {
        let focused = render_app_config_rows(app);
        let caret = focused
            .iter()
            .find(|r| r.contains('❯'))
            .unwrap_or_else(|| panic!("no caret row:\n{}", focused.join("\n")));
        assert_eq!(
            caret,
            &caret_over(own_text),
            "up stop {i} lands on its own row:\n{}",
            focused.join("\n")
        );
        if i > 0 {
            handle_key(app, crate::testutil::key(KeyCode::Up));
        }
    }
}

/// `↵` on an entry expands it in place (no recompute); the very next `↓` walks
/// into that entry's first field. The walk must never hold a stale stop list:
/// on a stale cache the published rows and the walk's stops disagree in length
/// and the step is dropped.
#[test]
fn expanding_an_entry_in_place_steps_down_into_its_first_field() {
    use crate::tui::app::{
        ServicesFocus, ShuntEntryKey, ShuntFieldKey, ShuntFocus, ShuntFocusLine, UpstreamFieldKind,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![shorthand_up("a", "claude_oauth")];
    let mut app = config_view_app_with_enabled(view, std::collections::HashSet::new());
    render_tall(&app);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Entry(ShuntEntryKey::Upstream {
            name: "a".into(),
            occurrence: 0,
        })));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    render_tall(&app);
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Field {
                key: ShuntFieldKey::Upstream {
                    name,
                    field: UpstreamFieldKind::Name,
                    ..
                }
            }) if name == "a"
        ),
        "down after an in-place expand lands on the entry's name field"
    );
    assert_walk_lands_on_own_rows(&mut app);
}

/// `↵` on an expanded entry collapses it in place; the next `↓` skips the
/// folded fields and lands on the following entry.
#[test]
fn collapsing_an_entry_in_place_steps_over_its_folded_fields() {
    use crate::tui::app::{ServicesFocus, ShuntEntryKey, ShuntFocus, ShuntFocusLine};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![
        shorthand_up("a", "claude_oauth"),
        shorthand_up("b", "api_key"),
    ];
    let mut app = config_view_app_with_enabled(
        view,
        std::collections::HashSet::from([ShuntEntryKey::Upstream {
            name: "a".into(),
            occurrence: 0,
        }]),
    );
    render_tall(&app);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Entry(ShuntEntryKey::Upstream {
            name: "a".into(),
            occurrence: 0,
        })));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    render_tall(&app);
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Entry {
                key: ShuntEntryKey::Upstream { name, .. }
            }) if name == "b"
        ),
        "down after an in-place collapse skips the folded fields onto the next entry"
    );
    assert_walk_lands_on_own_rows(&mut app);
}

/// `esc` on a field row that is not editing folds its entry and focuses the
/// entry row; `↓` then walks from there (never dropped on a stale stop list).
#[test]
fn esc_folding_an_entry_walks_from_the_entry_row() {
    use crate::tui::app::{
        ServicesFocus, ShuntEntryKey, ShuntFieldKey, ShuntFocus, ShuntFocusLine, UpstreamFieldKind,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![
        shorthand_up("a", "claude_oauth"),
        shorthand_up("b", "api_key"),
    ];
    let mut app = config_view_app_with_enabled(
        view,
        std::collections::HashSet::from([ShuntEntryKey::Upstream {
            name: "a".into(),
            occurrence: 0,
        }]),
    );
    render_tall(&app);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Field(ShuntFieldKey::Upstream {
            name: "a".into(),
            occurrence: 0,
            field: UpstreamFieldKind::Name,
        })));
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    render_tall(&app);
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Entry {
                key: ShuntEntryKey::Upstream { name, .. }
            }) if name == "a"
        ),
        "esc folds onto the entry row"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Entry {
                key: ShuntEntryKey::Upstream { name, .. }
            }) if name == "b"
        ),
        "down walks from the entry row after an esc fold"
    );
    assert_walk_lands_on_own_rows(&mut app);
}

/// With a read-only bind, the render publishes exactly one rendered row per
/// focus stop — no stop for the bind row the walk omits — and each stop's own
/// row (by its text) sits at its published row: the caret lands there, never
/// on the row above or below.
#[test]
fn the_focus_walk_lands_on_its_own_rows_with_a_read_only_bind() {
    use crate::tui::app::{ServicesFocus, ShuntFieldKey, ShuntFocus};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.server.bind_override = BindOverride::Set("SHUNT_SERVER__BIND".to_string());
    view.upstreams = vec![shorthand_up("anthropic", "claude_oauth")];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "anthropic".into(),
        occurrence: 0,
    }]);
    let mut app = config_view_app(view.clone(), expanded.clone());
    render_tall(&app);
    let stops = crate::tui::app::config_field_stops(
        &view,
        &expanded,
        &app.services.shunt_drafts,
        &app.services.shunt_tokens,
    );

    // Each stop's own line at rest, in card order (the read-only bind row and
    // the eyebrows render but are not stops).
    let own_lines = [
        "  default_provider  anthropic",
        "  ▼ anthropic       claude_oauth",
        "    name            anthropic",
        "    provider        —",
        "    kind            —",
        "    base_url        —",
        "    auth mode       passthrough  api_key  chatgpt_oauth",
        "    accounts        —",
        "    effort          —",
        "    service_tier    —",
        "    remove upstream",
        "  + add upstream",
        "  + add model",
        "  + add route",
        "  + add token",
    ];
    assert_eq!(
        stops.len(),
        own_lines.len(),
        "the stop list matches the card"
    );

    // The blurred (list-focused) render holds every stop's own line at its
    // true position; the published geometry must name exactly those rows.
    app.services.focus = ServicesFocus::List;
    let rest = render_app_config_rows(&app);
    let offset = rest
        .iter()
        .position(|r| r.starts_with("binary"))
        .expect("the binary row");
    let published = app.services.shunt_stop_rows.borrow().clone();
    assert_eq!(
        published.len(),
        stops.len(),
        "one rendered row per focus stop; the read-only bind row joins none"
    );
    for (i, own) in own_lines.iter().enumerate() {
        assert_eq!(
            &rest[offset + published[i]],
            own,
            "stop {i} publishes its own row (its text, not just a count):\n{}",
            rest.join("\n")
        );
    }

    // Walk to each stop: the caret lands on that stop's own published row.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    for (i, stop) in stops.iter().enumerate() {
        assert_eq!(
            app.services.focused_shunt_focus().map(|s| s.kind()),
            Some(stop.kind()),
            "walk step {i} lands on its own stop"
        );
        let focused = render_app_config_rows(&app);
        let caret_row = focused
            .iter()
            .position(|r| r.contains('❯'))
            .unwrap_or_else(|| panic!("no caret row:\n{}", focused.join("\n")));
        assert_eq!(
            caret_row,
            offset + published[i],
            "the caret sits on stop {i}'s own row"
        );
        if i + 1 < stops.len() {
            handle_key(&mut app, crate::testutil::key(KeyCode::Down));
        }
    }
    assert!(
        matches!(
            stops.first(),
            Some(ShuntFocus::Field {
                key: ShuntFieldKey::DefaultProvider
            })
        ),
        "the bind row is read-only, so the walk opens on default_provider"
    );
}

/// The walk and the render agree after opening and folding a draft: every stop
/// (the draft field rows and the `save <kind>` row included) lands on its own
/// rendered text, and folding drops them back out of the walk — never a stale
/// stop list or a count-only check.
#[test]
fn the_walk_agrees_after_opening_and_folding_a_draft() {
    use crate::tui::app::{ServicesFocus, ShuntAddKind, ShuntFocusLine};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![shorthand_up("a", "api_key")];
    let mut app = config_view_app_with_enabled(view, std::collections::HashSet::new());
    render_tall(&app);

    // Open the upstream draft from its `+ add` row.
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Add(ShuntAddKind::Upstream)));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        app.services.shunt_drafts.is_open(ShuntAddKind::Upstream),
        "enter opens the draft"
    );
    assert_walk_lands_on_own_rows(&mut app);

    // Fold it back with `↵` on the `+ add` row (now focus is back on it).
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Add(ShuntAddKind::Upstream)));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        !app.services.shunt_drafts.is_open(ShuntAddKind::Upstream),
        "enter on the open `+ add` row folds the draft"
    );
    assert_walk_lands_on_own_rows(&mut app);

    // Re-open, then `esc` at rest on a draft field row folds the draft too.
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Add(ShuntAddKind::Upstream)));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    app.services.set_shunt_focus(Some(ShuntFocusLine::Field(
        crate::tui::app::ShuntFieldKey::Draft {
            kind: ShuntAddKind::Upstream,
            field: crate::tui::app::ShuntDraftField::Upstream(
                crate::tui::app::UpstreamFieldKind::Name,
            ),
        },
    )));
    render_tall(&app);
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(
        !app.services.shunt_drafts.is_open(ShuntAddKind::Upstream),
        "esc at rest on a draft field row folds the draft"
    );
    assert_walk_lands_on_own_rows(&mut app);

    // Re-open, then commit `auth mode` = `api_key` on the draft: the one draft
    // change that reshapes the row set mid-draft (the mode-dependent rows
    // appear). The walk and the render must agree on the new rows at once.
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Add(ShuntAddKind::Upstream)));
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    app.services.set_shunt_focus(Some(ShuntFocusLine::Field(
        crate::tui::app::ShuntFieldKey::Draft {
            kind: ShuntAddKind::Upstream,
            field: crate::tui::app::ShuntDraftField::Upstream(
                crate::tui::app::UpstreamFieldKind::AuthMode,
            ),
        },
    )));
    render_tall(&app);
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    for c in "claude_oauth".chars() {
        handle_key(&mut app, crate::testutil::key(KeyCode::Char(c)));
    }
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_walk_lands_on_own_rows(&mut app);
}

/// While a cycle save runs, the row shows the same saving shape as a typed
/// save: the spinner in the caret slot, the value being saved under the
/// shimmer, and a dim `checking` trail — not the pre-save held value.
#[test]
fn a_cycle_save_renders_like_a_typed_save() {
    use crate::tui::app::{ShuntFieldKey, ShuntSave, ShuntSaveKey, UpstreamFieldKind};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("a")),
        provider: None,
        kind: Some(w("anthropic")),
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    }]);
    let mut app = config_view_app(view, expanded);
    let key = ShuntFieldKey::Upstream {
        name: "a".into(),
        occurrence: 0,
        field: UpstreamFieldKind::Kind,
    };
    app.services.shunt_save = Some(ShuntSave {
        key: ShuntSaveKey::Field(key),
        started: std::time::Instant::now(),
        value: Some("responses".into()),
        entry: None,
    });
    app.tick_count = 0;
    let rows = render_app_config_rows(&app);
    let kind_row = rows
        .iter()
        .find(|r| r.contains("kind"))
        .unwrap_or_else(|| panic!("no kind row:\n{}", rows.join("\n")));
    assert_eq!(
        kind_row.trim(),
        "⠋   kind            responses  checking",
        "a cycle save renders the value being saved with a dim checking trail"
    );
}

/// The saving row's `checking` trail is `TEXT_DIM`, not `TEXT_FAINT` or the
/// value's accent.
#[test]
fn a_saving_row_trails_checking_in_text_dim() {
    use crate::tui::app::{ShuntFieldKey, ShuntSave, ShuntSaveKey, UpstreamFieldKind};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("a")),
        provider: None,
        kind: Some(w("anthropic")),
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    }]);
    let mut app = config_view_app(view, expanded);
    app.services.shunt_save = Some(ShuntSave {
        key: ShuntSaveKey::Field(ShuntFieldKey::Upstream {
            name: "a".into(),
            occurrence: 0,
            field: UpstreamFieldKind::Kind,
        }),
        started: std::time::Instant::now(),
        value: Some("responses".into()),
        entry: None,
    });
    app.tick_count = 0;
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let buf = term.backend().buffer().clone();
    let cells = crate::testutil::needle_cells(&buf, "checking").expect("checking renders");
    let dim = crate::tui::theme::text_dim_color();
    assert!(
        cells.iter().all(|c| c.fg == dim),
        "checking trails in TEXT_DIM, got {:?}",
        cells.first().map(|c| c.fg)
    );
}

/// A focused unset cycle row still reads `—` (the approved unset form), the
/// caret in the gutter, never the bare option run.
#[test]
fn a_focused_unset_cycle_row_reads_an_em_dash() {
    use crate::tui::app::{ServicesFocus, ShuntFieldKey, ShuntFocusLine, UpstreamFieldKind};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("a")),
        provider: None,
        kind: None,
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    }]);
    let mut app = config_view_app(view, expanded);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Field(ShuntFieldKey::Upstream {
            name: "a".into(),
            occurrence: 0,
            field: UpstreamFieldKind::Provider,
        })));
    render_tall(&app);
    let rows = render_app_config_rows(&app);
    let provider = rows
        .iter()
        .find(|r| r.contains('❯'))
        .unwrap_or_else(|| panic!("no focused row:\n{}", rows.join("\n")));
    assert_eq!(
        provider,
        "❯   provider        —",
        "the focused unset cycle row reads —:\n{}",
        rows.join("\n")
    );
}

/// A focused wrapped cycle run keeps the focus treatment on every line: the
/// bracket sits on the picked option wherever it sits, and the `BG_HOVER` tint
/// spans the continuation lines too.
#[test]
fn a_focused_wrapped_cycle_row_highlights_every_line() {
    use crate::tui::app::{ShuntFieldKey, ShuntFocus, UpstreamFieldKind};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("a")),
        provider: Some(w("cursor")),
        kind: None,
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    }]);
    let mut app = config_view_app(view, expanded);
    render_tall(&app);
    // Walk to the provider field: bind → default_provider → entry → name →
    // provider.
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Field {
                key: ShuntFieldKey::Upstream {
                    field: UpstreamFieldKind::Provider,
                    ..
                }
            })
        ),
        "the provider field holds focus"
    );
    let rows = render_app_config_rows(&app);
    let provider_idx = rows
        .iter()
        .position(|r| r.contains('❯'))
        .expect("the focused provider row");
    // The held value `cursor` sits on the run's continuation line, so its
    // bracket must render there — not on the first line.
    assert_eq!(
        &rows[provider_idx..provider_idx + 3],
        &[
            "❯   provider        anthropic  codex  openai  xai  grok  kimi".to_string(),
            "                    [cursor]  kimi-code  zhipu  minimax-cn".to_string(),
            "                    opencode".to_string(),
        ],
        "the bracket sits on the continuation line:\n{}",
        rows.join("\n")
    );
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let buf = term.backend().buffer().clone();
    // The `❯` caret, the bracketed held option and an option on the run's
    // continuation line all sit on the BG_HOVER tint: the focus treatment spans
    // every line of the wrapped run.
    let hover = crate::tui::theme::bg_hover();
    for needle in ["❯", "[cursor]", "opencode"] {
        let cells = crate::testutil::needle_cells(&buf, needle)
            .unwrap_or_else(|| panic!("`{needle}` missing"));
        assert!(
            cells.iter().any(|c| c.bg == hover),
            "`{needle}` carries BG_HOVER on the focused wrapped cycle run"
        );
    }
}

/// A typed editor places the native cursor at the caret column of the value,
/// on the row that renders it.
#[test]
fn a_typed_editor_places_the_native_cursor_at_the_caret() {
    use crate::tui::app::{
        InputState, ServicesFocus, ShuntFieldEdit, ShuntFieldEditState, ShuntFieldKey,
        ShuntFocusLine, UpstreamFieldKind,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("a")),
        provider: None,
        kind: None,
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    }]);
    let key = ShuntFieldKey::Upstream {
        name: "a".into(),
        occurrence: 0,
        field: UpstreamFieldKind::Effort,
    };
    let mut app = config_view_app(view, expanded);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Field(key.clone())));
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key,
        edit: ShuntFieldEdit::Typed {
            input: InputState::new("hi"),
            saved: None,
        },
        refusal: None,
    });
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let caret = term.get_cursor_position().expect("the native cursor");
    let buf = term.backend().buffer().clone();
    let rows = crate::testutil::buffer_rows(&buf);
    let effort_idx = rows
        .iter()
        .position(|r| r.contains("effort"))
        .expect("the effort row renders");
    assert_eq!(
        caret.y as usize,
        effort_idx,
        "the caret sits on the edited row:\n{}",
        rows.join("\n")
    );
    // The caret column is the end of the typed value, measured against the
    // rendered value rather than an arithmetic sum.
    let w = buf.area.width as usize;
    let hi_x = (0..w)
        .find(|&x| buf.content[effort_idx * w + x].symbol() == "h")
        .expect("the typed value renders");
    assert_eq!(
        caret.x as usize,
        hi_x + 2,
        "the caret sits after the typed value"
    );
}

/// A control character in the typed value renders escaped, and the native
/// cursor sits after the escaped text — the shared `head_cols` counts what the
/// editor body draws, so the caret never drifts right of it.
#[test]
fn a_control_character_value_keeps_the_shunt_editor_caret_aligned() {
    use crate::tui::app::{
        InputState, ServicesFocus, ShuntFieldEdit, ShuntFieldEditState, ShuntFieldKey,
        ShuntFocusLine, UpstreamFieldKind,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("a")),
        provider: None,
        kind: None,
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    }]);
    let key = ShuntFieldKey::Upstream {
        name: "a".into(),
        occurrence: 0,
        field: UpstreamFieldKind::Effort,
    };
    let mut app = config_view_app(view, expanded);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Field(key.clone())));
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key,
        edit: ShuntFieldEdit::Typed {
            input: InputState::new("x\ty"),
            saved: None,
        },
        refusal: None,
    });
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let caret = term.get_cursor_position().expect("the native cursor");
    let buf = term.backend().buffer().clone();
    let rows = crate::testutil::buffer_rows(&buf);
    let effort_idx = rows
        .iter()
        .position(|r| r.contains("effort"))
        .expect("the effort row renders");
    // `x\ty` renders as the escaped `x\ty` (4 cells: x, \, t, y); the caret
    // sits right after the `y`.
    let w = buf.area.width as usize;
    let x_x = (0..w)
        .find(|&x| buf.content[effort_idx * w + x].symbol() == "x")
        .expect("the escaped value renders");
    assert_eq!(
        caret.x as usize,
        x_x + 4,
        "the caret sits after the escaped value:\n{}",
        rows.join("\n")
    );
}

/// A saving value carries the shimmer crest: on the full tier, at mid-sweep,
/// the value's cells are tinted away from the plain accent.
#[test]
fn a_saving_value_carries_the_shimmer_crest() {
    use crate::tui::app::{ShuntFieldKey, ShuntSave, ShuntSaveKey, UpstreamFieldKind};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("a")),
        provider: None,
        kind: None,
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    }]);
    let key = ShuntFieldKey::Upstream {
        name: "a".into(),
        occurrence: 0,
        field: UpstreamFieldKind::Effort,
    };
    let mut app = config_view_app(view, expanded);
    app.services.shunt_save = Some(ShuntSave {
        key: ShuntSaveKey::Field(key),
        started: std::time::Instant::now() - std::time::Duration::from_millis(200),
        value: Some("high".into()),
        entry: None,
    });
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let buf = term.backend().buffer().clone();
    let cells = crate::testutil::needle_cells(&buf, "high").expect("the saving value");
    assert!(
        cells
            .iter()
            .any(|c| c.fg != crate::tui::theme::accent_color()),
        "the shimmer crest tints the value away from the plain accent"
    );
}

/// Focus is held by the entry's identity, never a line index: a recompute that
/// reorders the upstreams keeps focus on the same entry.
#[test]
fn config_focus_identity_holds_across_a_reorder() {
    use crate::tui::app::{ShuntFocus, ShuntFocusLine};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![
        shorthand_up("anthropic", "claude_oauth"),
        shorthand_up("openai", "api_key"),
    ];
    let mut app = config_view_app(view.clone(), std::collections::HashSet::new());
    render_tall(&app);

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Entry(ShuntEntryKey::Upstream {
            name: "openai".into(),
            occurrence: 0,
        })));
    assert_eq!(
        app.services.shunt_focus_kind(),
        Some(ShuntFocusLine::Entry(ShuntEntryKey::Upstream {
            name: "openai".into(),
            occurrence: 0,
        })),
        "focus holds on openai before the reorder"
    );

    // A recompute that reorders the upstreams: openai now first. The held
    // identity stays, so focus re-resolves to openai at its new position.
    view.upstreams.reverse();
    let mut card = crate::tui::app::shunt_check(&shunt_slot_full(), true, None);
    card.shunt_focus.extend(crate::tui::app::config_field_stops(
        &view,
        &app.services.shunt_expanded,
        &app.services.shunt_drafts,
        &app.services.shunt_tokens,
    ));
    app.services.checks = vec![card];
    app.services.shunt_config = Some(ShuntConfigRead::Ok(Box::new(view)));
    render_tall(&app);

    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Entry {
                key: ShuntEntryKey::Upstream { name, .. }
            }) if name == "openai"
        ),
        "focus follows the entry by identity across the reorder"
    );
}

/// The expanded set drops a key whose entry left the config, and keeps the
/// surviving entries' keys. A failed read never reaches the prune (the
/// recompute prunes only over a read view), so it keeps the set.
#[test]
fn the_config_expanded_set_drops_a_removed_entry() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![shorthand_up("kept", "api_key")];
    let mut expanded = std::collections::HashSet::from([
        ShuntEntryKey::Upstream {
            name: "kept".into(),
            occurrence: 0,
        },
        ShuntEntryKey::Upstream {
            name: "gone".into(),
            occurrence: 0,
        },
    ]);
    crate::tui::app::prune_shunt_expanded(&view, &mut expanded);
    assert_eq!(
        expanded,
        std::collections::HashSet::from([ShuntEntryKey::Upstream {
            name: "kept".into(),
            occurrence: 0
        }]),
        "the removed entry's key drops, the surviving key stays"
    );
}

/// The footer advertises `space/↵ expand` on a collapsed focused entry and
/// `space/↵ collapse` once it is expanded, derived from the focused stop —
/// pinned by equality on the whole hint vec.
#[test]
fn the_config_footer_hints_expand_then_collapse() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![shorthand_up("anthropic", "claude_oauth")];
    let mut app = config_view_app(view, std::collections::HashSet::new());
    render_tall(&app);

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    assert_eq!(
        super::super::footer::services_hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("space/↵", "expand"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "a collapsed focused entry advertises expand"
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert_eq!(
        super::super::footer::services_hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("space/↵", "collapse"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "an expanded focused entry advertises collapse"
    );
}

/// A config read failure renders no groups and one `config  not read (<reason>)`
/// line after the card's lines; a shape refusal renders `config  not editable`
/// and its sentence on a faint `└` line.
#[test]
fn the_shunt_config_read_failure_names_the_reason_or_the_refusal() {
    use crate::tui::app::UnreadConfig;
    let _home = crate::testutil::HomeSandbox::new();
    let mut card = crate::tui::app::shunt_check(&shunt_slot_full(), true, None);
    card.shunt_focus = Vec::new();
    let mut app = app_with(card);
    app.services.shunt_config = Some(ShuntConfigRead::Unread(UnreadConfig::Unparsed));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 40)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert_eq!(
        config_detail_rows(&rows),
        vec![
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "config  not read (does not parse)",
        ],
        "an unparsed config renders the reason line: {}",
        rows.join("\n")
    );

    let mut card = crate::tui::app::shunt_check(&shunt_slot_full(), true, None);
    card.shunt_focus = Vec::new();
    let mut app = app_with(card);
    app.services.shunt_config = Some(ShuntConfigRead::Refused(
        "a shape clauth does not edit".to_string(),
    ));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 40)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert_eq!(
        config_detail_rows(&rows),
        vec![
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "config  not editable",
            "  └ a shape clauth does not edit",
        ],
        "a shape refusal renders the not-editable line and its sentence: {}",
        rows.join("\n")
    );
}

/// An env file the view cannot read keeps the groups: one `env file  not read
/// (<reason>)` line above them, and the bind row reads `env file not read`.
#[test]
fn the_shunt_config_renders_the_env_file_failure_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.server.bind_override = BindOverride::Unknown(EnvFileFailure::Missing);
    assert_config_detail(
        view,
        std::collections::HashSet::new(),
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "env file  not read (missing)",
            "SERVER",
            "  bind              127.0.0.1:3001  env file not read",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  + add upstream",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// Two models sharing one id render as distinct rows: expanding the second
/// shows the second's fields, never the first's.
#[test]
fn the_shunt_config_renders_two_same_id_models_distinctly() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.models = vec![
        ModelView {
            id: Some(w("m")),
            display_name: Some(w("First")),
            upstream_model: None,
        },
        ModelView {
            id: Some(w("m")),
            display_name: Some(w("Second")),
            upstream_model: None,
        },
    ];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Model {
        id: "m".into(),
        occurrence: 1,
    }]);
    assert_config_detail(
        view,
        expanded,
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  + add upstream",
            "MODELS",
            "  ▶ m",
            "  ▼ m",
            "    id              m",
            "    display_name    Second",
            "  + add model",
            "ROUTES",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
}

/// Two models sharing one id are distinct rows: `↓` moves between them and
/// `↵` expands only the focused one.
#[test]
fn the_config_focus_walk_reaches_two_same_id_models() {
    use crate::tui::app::{ShuntFocus, ShuntFocusLine};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.models = vec![
        ModelView {
            id: Some(w("m")),
            display_name: None,
            upstream_model: None,
        },
        ModelView {
            id: Some(w("m")),
            display_name: None,
            upstream_model: None,
        },
    ];
    let mut app = config_view_app(view, std::collections::HashSet::new());
    render_tall(&app);

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    // The server fields lead the walk; focus the second same-id model directly.
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Entry(ShuntEntryKey::Model {
            id: "m".into(),
            occurrence: 1,
        })));
    render_tall(&app);
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Entry {
                key: ShuntEntryKey::Model { id, occurrence: 1 }
            }) if id == "m"
        ),
        "focus lands on the second same-id model"
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        app.services.shunt_expanded.contains(&ShuntEntryKey::Model {
            id: "m".into(),
            occurrence: 1
        }),
        "enter expands only the focused model"
    );
    assert!(
        !app.services.shunt_expanded.contains(&ShuntEntryKey::Model {
            id: "m".into(),
            occurrence: 0
        }),
        "the first same-id model stays collapsed"
    );
}

/// Two upstreams sharing a name are distinct rows: expanding the second shows
/// the second's summary and fields, never the first's.
#[test]
fn the_shunt_config_renders_two_same_name_upstreams_distinctly() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![
        shorthand_up("a", "api_key"),
        shorthand_up("a", "claude_oauth"),
    ];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 1,
    }]);
    assert_eq!(
        render_config_section(view, expanded, W),
        vec![
            "SERVER".to_string(),
            "  bind              127.0.0.1:3001".to_string(),
            "  default_provider  anthropic".to_string(),
            "UPSTREAMS".to_string(),
            format!("  ▶ a{}api_key", " ".repeat(15)),
            format!("  ▼ a{}claude_oauth", " ".repeat(15)),
            "    name            a".to_string(),
            "    provider        —".to_string(),
            "    kind            —".to_string(),
            "    base_url        —".to_string(),
            "    auth mode       passthrough  api_key  chatgpt_oauth".to_string(),
            "                    claude_oauth  xai_oauth  cursor_oauth".to_string(),
            "                    kimi_oauth  google_oauth  antigravity_oauth".to_string(),
            "                    none".to_string(),
            "    accounts        —".to_string(),
            "    effort          —".to_string(),
            "    service_tier    —".to_string(),
            "  + add upstream".to_string(),
            "MODELS".to_string(),
            "  + add model".to_string(),
            "ROUTES".to_string(),
            "  + add route".to_string(),
            "CLIENT TOKENS".to_string(),
            "  + add token".to_string(),
        ],
        "the second same-name upstream draws its own summary and fields"
    );
}

/// Two nameless upstreams (keyed by the empty name plus occurrence) are
/// distinct rows too: expanding the second shows the second's fields.
#[test]
fn the_shunt_config_renders_two_nameless_upstreams_distinctly() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![nameless_up("api_key"), nameless_up("claude_oauth")];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: String::new(),
        occurrence: 1,
    }]);
    // The empty name pads to the key column (14 + 2 gutter), so the entry rows
    // open their summary at the shared value column, col 20.
    assert_eq!(
        render_config_section(view, expanded, W),
        vec![
            "SERVER".to_string(),
            "  bind              127.0.0.1:3001".to_string(),
            "  default_provider  anthropic".to_string(),
            "UPSTREAMS".to_string(),
            format!("  ▶ {}api_key", " ".repeat(16)),
            format!("  ▼ {}claude_oauth", " ".repeat(16)),
            "    name            —".to_string(),
            "    provider        —".to_string(),
            "    kind            —".to_string(),
            "    base_url        —".to_string(),
            "    auth mode       passthrough  api_key  chatgpt_oauth".to_string(),
            "                    claude_oauth  xai_oauth  cursor_oauth".to_string(),
            "                    kimi_oauth  google_oauth  antigravity_oauth".to_string(),
            "                    none".to_string(),
            "    accounts        —".to_string(),
            "    effort          —".to_string(),
            "    service_tier    —".to_string(),
            "  + add upstream".to_string(),
            "MODELS".to_string(),
            "  + add model".to_string(),
            "ROUTES".to_string(),
            "  + add route".to_string(),
            "CLIENT TOKENS".to_string(),
            "  + add token".to_string(),
        ],
        "the second nameless upstream draws its own fields"
    );
}

/// Two upstreams sharing a name are distinct stops: `↓` moves between them and
/// `↵` expands only the focused one.
#[test]
fn the_config_focus_walk_reaches_two_same_name_upstreams() {
    use crate::tui::app::{ShuntFocus, ShuntFocusLine};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![
        shorthand_up("a", "api_key"),
        shorthand_up("a", "claude_oauth"),
    ];
    let mut app = config_view_app(view, std::collections::HashSet::new());
    render_tall(&app);

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Entry(ShuntEntryKey::Upstream {
            name: "a".into(),
            occurrence: 1,
        })));
    render_tall(&app);
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Entry {
                key: ShuntEntryKey::Upstream { name, occurrence: 1 }
            }) if name == "a"
        ),
        "focus lands on the second same-name upstream"
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        app.services
            .shunt_expanded
            .contains(&ShuntEntryKey::Upstream {
                name: "a".into(),
                occurrence: 1
            }),
        "enter expands only the focused upstream"
    );
    assert!(
        !app.services
            .shunt_expanded
            .contains(&ShuntEntryKey::Upstream {
                name: "a".into(),
                occurrence: 0
            }),
        "the first same-name upstream stays collapsed"
    );
}

/// Two nameless upstreams are distinct stops: `↓` moves between them and `↵`
/// expands only the focused one.
#[test]
fn the_config_focus_walk_reaches_two_nameless_upstreams() {
    use crate::tui::app::{ShuntFocus, ShuntFocusLine};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![nameless_up("api_key"), nameless_up("claude_oauth")];
    let mut app = config_view_app(view, std::collections::HashSet::new());
    render_tall(&app);

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Entry(ShuntEntryKey::Upstream {
            name: String::new(),
            occurrence: 1,
        })));
    render_tall(&app);
    assert!(
        matches!(
            app.services.focused_shunt_focus(),
            Some(ShuntFocus::Entry {
                key: ShuntEntryKey::Upstream { name, occurrence: 1 }
            }) if name.is_empty()
        ),
        "focus lands on the second nameless upstream"
    );

    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    assert!(
        app.services
            .shunt_expanded
            .contains(&ShuntEntryKey::Upstream {
                name: String::new(),
                occurrence: 1
            }),
        "enter expands only the focused nameless upstream"
    );
    assert!(
        !app.services
            .shunt_expanded
            .contains(&ShuntEntryKey::Upstream {
                name: String::new(),
                occurrence: 0
            }),
        "the first nameless upstream stays collapsed"
    );
}

/// At a narrow width a long upstream name, summary and route model each
/// trail-ellipsize at their slot (`…`), never clip at the pane edge.
#[test]
fn the_shunt_config_ellipsizes_long_names_summaries_and_route_labels() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("abcdefghijklmnopqrstuvwxyz")),
        provider: None,
        kind: None,
        base_url: None,
        auth: Some(AuthView {
            form: AuthForm::Inline,
            mode: Some(w("antigravity_oauth")),
            account: None,
            accounts: Some(vec![
                AccountView::Name(w("alice")),
                AccountView::Name(w("bob")),
            ]),
            env: None,
            header: None,
        }),
        effort: None,
        service_tier: None,
    }];
    view.routes = vec![RouteView {
        position: 1,
        model: Some(w(
            "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
        )),
        provider: Some(w("anthropic")),
        upstream_model: None,
        effort: None,
        service_tier: None,
    }];
    // 60 cols is still desktop (the horizontal split), the detail pane 36 wide:
    // the name slot (14) ellipsizes the 26-char name, the value room (16) the
    // 30-char summary, the route label room (32) the model slug alone while the
    // arrow and provider stay whole.
    assert_eq!(
        render_config_section(view, std::collections::HashSet::new(), 60),
        vec![
            "SERVER".to_string(),
            "  bind              127.0.0.1:3001".to_string(),
            "  default_provider  anthropic".to_string(),
            "UPSTREAMS".to_string(),
            "  ▶ abcdefghijklm…  antigravity_oau…".to_string(),
            "  + add upstream".to_string(),
            "MODELS".to_string(),
            "  + add model".to_string(),
            "ROUTES".to_string(),
            "  ▶ 1  0123456789abcdef… → anthropic".to_string(),
            "  + add route".to_string(),
            "CLIENT TOKENS".to_string(),
            "  + add token".to_string(),
        ],
        "long names, summaries and route labels ellipsize at their slot"
    );
}

/// A collapsed route row keeps the position, the arrow and the provider whole
/// and shrinks the model slug alone with a trailing `…` — the arrow and the
/// provider never drop off the pane edge.
#[test]
fn the_shunt_config_keeps_the_route_provider_when_the_model_shrinks() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.routes = vec![RouteView {
        position: 2,
        model: Some(w("a-really-long-model-slug-for-truncation-checks")),
        provider: Some(w("openai")),
        upstream_model: None,
        effort: None,
        service_tier: None,
    }];
    assert_eq!(
        render_config_section(view, std::collections::HashSet::new(), 80),
        vec![
            "SERVER".to_string(),
            "  bind              127.0.0.1:3001".to_string(),
            "  default_provider  anthropic".to_string(),
            "UPSTREAMS".to_string(),
            "  + add upstream".to_string(),
            "MODELS".to_string(),
            "  + add model".to_string(),
            "ROUTES".to_string(),
            "  ▶ 2  a-really-long-model-slug-for-trunca… → openai".to_string(),
            "  + add route".to_string(),
            "CLIENT TOKENS".to_string(),
            "  + add token".to_string(),
        ],
        "the route label shrinks the model, keeping the arrow and provider"
    );
}

/// A provider that leaves the model no room keeps a `…` in the model's slot
/// and gives up one more cell itself, so the row never reads as a route with
/// no model.
#[test]
fn the_shunt_config_marks_a_crowded_out_route_model_with_an_ellipsis() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.routes = vec![RouteView {
        position: 2,
        model: Some(w("opus")),
        provider: Some(w("abcdefghijklmnopqrstuvwxyz0123456789ABCDEF")),
        upstream_model: None,
        effort: None,
        service_tier: None,
    }];
    assert_eq!(
        render_config_section(view, std::collections::HashSet::new(), 80),
        vec![
            "SERVER".to_string(),
            "  bind              127.0.0.1:3001".to_string(),
            "  default_provider  anthropic".to_string(),
            "UPSTREAMS".to_string(),
            "  + add upstream".to_string(),
            "MODELS".to_string(),
            "  + add model".to_string(),
            "ROUTES".to_string(),
            "  ▶ 2  … → abcdefghijklmnopqrstuvwxyz0123456789ABCD…".to_string(),
            "  + add route".to_string(),
            "CLIENT TOKENS".to_string(),
            "  + add token".to_string(),
        ],
        "the crowded-out model keeps a `…` and the provider shrinks by one more cell"
    );
}

/// An absent model crowded out by the provider still reads `—`, never the `…`
/// of a cut slug.
#[test]
fn the_shunt_config_marks_an_absent_crowded_route_model_with_an_em_dash() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.routes = vec![RouteView {
        position: 2,
        model: None,
        provider: Some(w("abcdefghijklmnopqrstuvwxyz0123456789ABCDEF")),
        upstream_model: None,
        effort: None,
        service_tier: None,
    }];
    assert_eq!(
        render_config_section(view, std::collections::HashSet::new(), 80),
        vec![
            "SERVER".to_string(),
            "  bind              127.0.0.1:3001".to_string(),
            "  default_provider  anthropic".to_string(),
            "UPSTREAMS".to_string(),
            "  + add upstream".to_string(),
            "MODELS".to_string(),
            "  + add model".to_string(),
            "ROUTES".to_string(),
            "  ▶ 2  — → abcdefghijklmnopqrstuvwxyz0123456789ABCD…".to_string(),
            "  + add route".to_string(),
            "CLIENT TOKENS".to_string(),
            "  + add token".to_string(),
        ],
        "an absent model keeps its `—` when the provider crowds it out"
    );
}

/// At a narrow pane the bind row's dim suffix (`set by <variable>` / `env file
/// not read`) trail-ellipsizes in the room left after the value and its 2-space
/// gap, never clipping past the pane edge; when the value itself overflows, it
/// ellipsizes and the suffix drops whole.
#[test]
fn the_shunt_config_ellipsizes_the_bind_suffix_at_a_narrow_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut set = base_view();
    set.server.bind_override = BindOverride::Set("SHUNT_SERVER__BIND".to_string());
    assert_eq!(
        render_config_section(set, std::collections::HashSet::new(), 80),
        vec![
            "SERVER".to_string(),
            "  bind              127.0.0.1:3001  set by SHUNT_SE…".to_string(),
            "  default_provider  anthropic".to_string(),
            "UPSTREAMS".to_string(),
            "  + add upstream".to_string(),
            "MODELS".to_string(),
            "  + add model".to_string(),
            "ROUTES".to_string(),
            "  + add route".to_string(),
            "CLIENT TOKENS".to_string(),
            "  + add token".to_string(),
        ],
        "the set-by suffix ellipsizes within the pane"
    );
    let mut unread = base_view();
    unread.server.bind_override = BindOverride::Unknown(EnvFileFailure::Missing);
    assert_eq!(
        render_config_section(unread, std::collections::HashSet::new(), 80),
        vec![
            "SERVER".to_string(),
            "  bind              127.0.0.1:3001  env file not re…".to_string(),
            "  default_provider  anthropic".to_string(),
            "UPSTREAMS".to_string(),
            "  + add upstream".to_string(),
            "MODELS".to_string(),
            "  + add model".to_string(),
            "ROUTES".to_string(),
            "  + add route".to_string(),
            "CLIENT TOKENS".to_string(),
            "  + add token".to_string(),
        ],
        "the env-file suffix ellipsizes within the pane"
    );
    let mut overflow = base_view();
    overflow.server.bind = Some(w("0123456789abcdef0123456789abcdef0"));
    overflow.server.bind_override = BindOverride::Set("SHUNT_SERVER__BIND".to_string());
    assert_eq!(
        render_config_section(overflow, std::collections::HashSet::new(), 80),
        vec![
            "SERVER".to_string(),
            "  bind              0123456789abcdef0123456789abcde…".to_string(),
            "  default_provider  anthropic".to_string(),
            "UPSTREAMS".to_string(),
            "  + add upstream".to_string(),
            "MODELS".to_string(),
            "  + add model".to_string(),
            "ROUTES".to_string(),
            "  + add route".to_string(),
            "CLIENT TOKENS".to_string(),
            "  + add token".to_string(),
        ],
        "an overflowing value ellipsizes and the suffix drops whole"
    );
}

/// Style pins: a collapsed-blurred entry renders its glyph and summary dim and
/// its label dim (no bold); an expanded-focused entry renders its glyph accent
/// and its label `TEXT + bold`; the focused group's eyebrow is `TEXT + bold`
/// while the detail pane holds focus, every other eyebrow (and that one once
/// the list holds focus) `TEXT_DIM + bold`, none underlined; field values
/// accent and `—` faint.
#[test]
fn the_shunt_config_entry_styles_follow_the_design_contract() {
    use crate::tui::app::{ServicesFocus, ShuntFocusLine};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut view = base_view();
    view.upstreams = vec![
        shorthand_up("anthropic", "claude_oauth"),
        shorthand_up("openai", "api_key"),
    ];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "openai".into(),
        occurrence: 0,
    }]);
    let mut app = config_view_app(view, expanded);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Entry(ShuntEntryKey::Upstream {
            name: "openai".into(),
            occurrence: 0,
        })));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let buf = term.backend().buffer().clone();

    // Collapsed-blurred entry: glyph dim, label dim (no bold), summary dim.
    let collapsed = crate::testutil::needle_cells(&buf, "▶ anthropic").expect("collapsed row");
    assert_eq!(
        collapsed[0].fg,
        super::theme::text_dim_color(),
        "collapsed glyph dim"
    );
    assert_eq!(
        collapsed[2].fg,
        super::theme::text_dim_color(),
        "blurred label dim"
    );
    assert!(
        !collapsed[2]
            .modifier
            .contains(ratatui::style::Modifier::BOLD),
        "blurred label not bold"
    );
    let summary = crate::testutil::needle_cells(&buf, "claude_oauth").expect("summary");
    assert_eq!(summary[0].fg, super::theme::text_dim_color(), "summary dim");

    // Expanded-focused entry: glyph accent, label TEXT + bold.
    let expanded_row = crate::testutil::needle_cells(&buf, "▼ openai").expect("expanded row");
    assert_eq!(
        expanded_row[0].fg,
        super::theme::accent_color(),
        "expanded glyph accent"
    );
    assert_eq!(
        expanded_row[2].fg,
        super::theme::text_color(),
        "focused label text"
    );
    assert!(
        expanded_row[2]
            .modifier
            .contains(ratatui::style::Modifier::BOLD),
        "focused label bold"
    );

    // A field value ACCENT (the SERVER default_provider), a typed absent key
    // `—` faint (the upstream's base_url).
    let value = crate::testutil::needle_cells(&buf, "anthropic").expect("value");
    assert_eq!(value[0].fg, super::theme::accent_color(), "value accent");
    let dash = crate::testutil::needle_cells(&buf, "base_url        —").expect("absent value");
    assert_eq!(
        dash[dash.len() - 1].fg,
        super::theme::text_faint_color(),
        "— faint"
    );

    // The focused group's eyebrow brightens; another group's stays at rest.
    assert_eyebrow(&buf, "UPSTREAMS", EYEBROW_ACTIVE);
    assert_eyebrow(&buf, "SERVER", EYEBROW_AT_REST);

    // The list taking focus back returns the held group's eyebrow to rest.
    app.services.focus = ServicesFocus::List;
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    assert_eyebrow(term.backend().buffer(), "UPSTREAMS", EYEBROW_AT_REST);
}

/// Assert every cell of the first `needle` run in `buf` renders `fg` + bold and
/// never underlined: an eyebrow's focus cue is its tier alone. The caller holds
/// a `Tier::Full` sandbox.
fn assert_eyebrow(buf: &ratatui::buffer::Buffer, needle: &str, fg: ratatui::style::Color) {
    let cells = crate::testutil::needle_cells(buf, needle)
        .unwrap_or_else(|| panic!("no `{needle}` eyebrow"));
    for cell in &cells {
        assert_eq!(cell.fg, fg, "the `{needle}` eyebrow's tier");
        assert!(
            cell.modifier.contains(ratatui::style::Modifier::BOLD),
            "the `{needle}` eyebrow is bold"
        );
        assert!(
            !cell.modifier.contains(ratatui::style::Modifier::UNDERLINED),
            "the `{needle}` eyebrow is never underlined"
        );
    }
}

/// A config holding both `[providers.*]` and `[[upstreams]]` (the mid-way state
/// of the by-hand conversion) renders the legacy providers row and note only —
/// no upstream rows, and no upstream focus stops, so the walk cannot land on a
/// row the render does not draw.
#[test]
fn the_shunt_config_legacy_form_hides_populated_upstreams() {
    use crate::tui::app::ShuntFocus;
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.form = ProviderForm::Legacy {
        providers: vec!["anthropic".to_string()],
    };
    view.upstreams = vec![shorthand_up("anthropic", "api_key")];
    view.routes = vec![RouteView {
        position: 1,
        model: Some(w("opus")),
        provider: Some(w("anthropic")),
        upstream_model: None,
        effort: None,
        service_tier: None,
    }];
    assert_config_detail(
        view.clone(),
        std::collections::HashSet::new(),
        &[
            "binary     /usr/local/bin/shunt",
            "config     /home/u/.clauth/gateway.toml",
            "version    0.49.1",
            "state      healthy",
            "reason     port busy",
            "pid        4242",
            "port       3001",
            "restarts   2",
            "last exit  exit 1",
            "",
            "SERVER",
            "  bind              127.0.0.1:3001",
            "  default_provider  anthropic",
            "UPSTREAMS",
            "  providers         anthropic",
            "  └ [providers.*] form; convert to [[upstreams]] by hand to edit",
            "MODELS",
            "  + add model",
            "ROUTES",
            "  ▶ 1  opus → anthropic",
            "  + add route",
            "CLIENT TOKENS",
            "  + add token",
        ],
    );
    assert!(matches!(
        &crate::tui::app::config_entry_stops(&view)[..],
        [ShuntFocus::Entry {
            key: ShuntEntryKey::Route(1)
        }]
    ));
}

/// The add/save/remove footer hints derive from the row's state: `+ add` reads
/// `add` closed and `collapse` open (folding drops the draft); `save`/`remove`
/// read `save`/`remove` at rest and shed them while their save checks — the
/// same predicate their handlers gate on.
#[test]
fn the_add_save_remove_footer_hints_derive_from_state() {
    use crate::tui::app::{ServicesFocus, ShuntAddKind, ShuntFocusLine, ShuntSave, ShuntSaveKey};
    let _home = crate::testutil::HomeSandbox::new();

    // `+ add` closed.
    let view = base_view();
    let mut app = config_draft_app(
        view,
        std::collections::HashSet::new(),
        ShuntDrafts::default(),
    );
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Add(ShuntAddKind::Upstream)));
    assert_eq!(
        super::super::footer::services_hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("↵", "add"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "`+ add` closed reads add"
    );

    // `+ add` open, `save` at rest, `save` saving — all on one open-draft app.
    let view = base_view();
    let mut app = config_draft_app(
        view,
        std::collections::HashSet::new(),
        ShuntDrafts {
            upstream: Some(crate::tui::app::UpstreamDraft::default()),
            ..ShuntDrafts::default()
        },
    );
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Add(ShuntAddKind::Upstream)));
    assert_eq!(
        super::super::footer::services_hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("↵", "collapse"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "`+ add` open reads collapse"
    );
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::SaveAdd(ShuntAddKind::Upstream)));
    assert_eq!(
        super::super::footer::services_hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("↵", "save"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "`save` at rest reads save"
    );
    app.services.shunt_save = Some(ShuntSave {
        key: ShuntSaveKey::SaveAdd(ShuntAddKind::Upstream),
        started: std::time::Instant::now(),
        value: None,
        entry: None,
    });
    assert_eq!(
        super::super::footer::services_hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "a saving `save` row offers no `↵ save`"
    );

    // `+ add` while its draft saves: the row is locked, so it sheds its
    // `↵` hint too (nothing it advertises acts).
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Add(ShuntAddKind::Upstream)));
    assert_eq!(
        super::super::footer::services_hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "a saving `+ add` row offers no `↵`"
    );
}

/// The add/save/remove footer hints: the `remove <kind>` row reads `↵ remove`
/// at rest and sheds it while its remove checks.
#[test]
fn the_remove_footer_hint_derives_from_state() {
    use crate::tui::app::{ServicesFocus, ShuntFocusLine, ShuntSave, ShuntSaveKey};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![shorthand_up("a", "api_key")];
    let entry = ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    };
    let expanded = std::collections::HashSet::from([entry.clone()]);
    let mut app = config_view_app(view, expanded);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Remove(entry.clone())));
    assert_eq!(
        super::super::footer::services_hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("↵", "remove"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "`remove` at rest reads remove"
    );
    app.services.shunt_save = Some(ShuntSave {
        key: ShuntSaveKey::Remove(entry.clone()),
        started: std::time::Instant::now(),
        value: None,
        entry: None,
    });
    assert_eq!(
        super::super::footer::services_hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "a saving `remove` row offers no `↵ remove`"
    );

    // The locked entry row (F-B1): while its remove checks, the entry's own
    // row sheds `space/↵ collapse|expand` — it advertises no key the lock
    // ignores. B's PB2-F1 plant reds this.
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Entry(entry)));
    assert_eq!(
        super::super::footer::services_hints(&app),
        vec![
            ("↑↓", "row"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "a removing entry's own row offers no `space/↵`"
    );
}

/// A saving `remove <kind>` row shows the C2 saving shape — the spinner, the
/// shimmer crest over its label and a dim `checking` trail — like `save
/// <kind>`.
#[test]
fn the_shunt_config_renders_a_saving_remove_row() {
    use crate::tui::app::{ShuntSave, ShuntSaveKey};
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![shorthand_up("a", "api_key")];
    let entry = ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    };
    let expanded = std::collections::HashSet::from([entry.clone()]);
    let mut app = config_view_app(view, expanded);
    app.tick_count = 0;
    app.services.shunt_save = Some(ShuntSave {
        key: ShuntSaveKey::Remove(entry),
        started: std::time::Instant::now(),
        value: None,
        entry: None,
    });
    let rows = render_app_config_rows(&app);
    let remove_row = rows
        .iter()
        .find(|r| r.contains("remove upstream"))
        .unwrap_or_else(|| panic!("no remove row:\n{}", rows.join("\n")));
    assert_eq!(
        remove_row, "⠋   remove upstream  checking",
        "the saving remove row shows spinner + shimmer + checking"
    );
}

/// The `+ add` row's label is ACCENT (the Master-detail `+ new` rule), never
/// the blurred field label's dim — B's P-B5 plant (label_style) reds this.
#[test]
fn the_add_row_label_is_accent() {
    use crate::tui::app::ShuntAddKind;
    let _home = crate::testutil::HomeSandbox::new();
    let view = base_view();
    let app = config_draft_app(
        view,
        std::collections::HashSet::new(),
        ShuntDrafts::default(),
    );
    let line = super::add_line(ShuntAddKind::Upstream, false, &app);
    assert_eq!(
        line.spans[1].style.fg,
        crate::tui::theme::accent().fg,
        "the `+ add` label is ACCENT, not the field label's dim"
    );
}

/// A saving `save <kind>` row shimmers its label into per-character spans at
/// the full tier (the crest moves across the name) — B's P-B4 plant (a static
/// accent label) reds this.
#[test]
fn the_saving_save_row_label_shimmers_into_per_char_spans() {
    use crate::tui::app::{ShuntAddKind, ShuntSave, ShuntSaveKey};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let view = base_view();
    let drafts = ShuntDrafts {
        upstream: Some(crate::tui::app::UpstreamDraft::default()),
        ..ShuntDrafts::default()
    };
    let mut app = config_draft_app(view, std::collections::HashSet::new(), drafts);
    app.services.shunt_save = Some(ShuntSave {
        key: ShuntSaveKey::SaveAdd(ShuntAddKind::Upstream),
        started: std::time::Instant::now() - std::time::Duration::from_millis(450),
        value: None,
        entry: None,
    });
    let rows = super::save_add_lines(ShuntAddKind::Upstream, false, &app, W as usize);
    let label_spans = rows[0].spans.len();
    assert!(
        label_spans > 5,
        "the shimmer splits the label into per-char spans, got {label_spans}"
    );
}

/// Ruling 9 (B-M-B2): the group eyebrow stays `TEXT + bold` while the focus
/// rests on its `+ add`, `save` or `remove` row, the way it does on an entry
/// or field. B's P2-B2 plant (dropping the `Add`/`SaveAdd`/`Remove` arms in
/// `focused_group`) reds this.
#[test]
fn the_group_eyebrow_stays_active_on_the_add_save_and_remove_rows() {
    use crate::tui::app::{
        ServicesFocus, ShuntAddKind, ShuntDrafts, ShuntFocusLine, UpstreamDraft,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);

    // `+ add upstream` focused (draft closed).
    let mut app = config_draft_app(
        base_view(),
        std::collections::HashSet::new(),
        ShuntDrafts::default(),
    );
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Add(ShuntAddKind::Upstream)));
    assert_upstreams_eyebrow_active(&app);

    // `save upstream` focused (draft open).
    let mut app = config_draft_app(
        base_view(),
        std::collections::HashSet::new(),
        ShuntDrafts {
            upstream: Some(UpstreamDraft::default()),
            ..ShuntDrafts::default()
        },
    );
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::SaveAdd(ShuntAddKind::Upstream)));
    assert_upstreams_eyebrow_active(&app);

    // `remove upstream` focused (entry expanded).
    let mut view = base_view();
    view.upstreams = vec![shorthand_up("a", "api_key")];
    let entry = ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    };
    let mut app = config_view_app(view, std::collections::HashSet::from([entry.clone()]));
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Remove(entry)));
    assert_upstreams_eyebrow_active(&app);
}

/// Render `app` and assert the `UPSTREAMS` eyebrow takes the focused group's
/// cue (`TEXT + bold`, never underlined).
fn assert_upstreams_eyebrow_active(app: &App) {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(W, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app, &[])).unwrap();
    assert_eyebrow(term.backend().buffer(), "UPSTREAMS", EYEBROW_ACTIVE);
}

/// Ruling 8 (B-M-B3): the `save <kind>` row takes the `create account`
/// treatment (ACCENT, bold while selected) and the `remove <kind>` row the
/// `delete account` treatment (DANGER, bold). B's P2-B3 plant (both to
/// `label_style`) reds this.
#[test]
fn the_save_and_remove_rows_take_the_house_colours() {
    use crate::tui::app::{ShuntAddKind, ShuntDrafts, UpstreamDraft};
    let _home = crate::testutil::HomeSandbox::new();

    let drafts = ShuntDrafts {
        upstream: Some(UpstreamDraft::default()),
        ..ShuntDrafts::default()
    };
    let app = config_draft_app(base_view(), std::collections::HashSet::new(), drafts);

    let selected = super::save_add_lines(ShuntAddKind::Upstream, true, &app, W as usize);
    assert_eq!(
        selected[0].spans[2].style.fg,
        crate::tui::theme::accent().fg,
        "the `save` label is ACCENT"
    );
    assert!(
        selected[0].spans[2]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "the selected `save` label is bold"
    );
    let blurred = super::save_add_lines(ShuntAddKind::Upstream, false, &app, W as usize);
    assert_eq!(
        blurred[0].spans[2].style.fg,
        crate::tui::theme::accent().fg,
        "the blurred `save` label stays ACCENT"
    );
    assert!(
        !blurred[0].spans[2]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "the blurred `save` label is not bold"
    );

    let mut view = base_view();
    view.upstreams = vec![shorthand_up("a", "api_key")];
    let entry = ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    };
    let app = config_view_app(view, std::collections::HashSet::from([entry.clone()]));
    let remove = super::remove_lines(&entry, true, &app, W as usize);
    assert_eq!(
        remove[0].spans[2].style.fg,
        crate::tui::theme::danger().fg,
        "the `remove` label is DANGER"
    );
    assert!(
        remove[0].spans[2]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "the `remove` label is bold at rest"
    );
}

// ── display-cell budgeting (part A fix round) ───────────────────────────────

/// `config_key` pads its label to the value column by cells, so a CJK or emoji
/// upstream name opens its value at the same column as an ASCII one instead of
/// pushing it right.
#[test]
fn a_shunt_config_key_pads_by_cells() {
    let ascii = super::config_key("name", 4);
    let cjk = super::config_key("日本語", 4);
    let emoji = super::config_key("🧭upstream", 4);
    assert_eq!(ratatui::text::Span::raw(&ascii).width(), 16);
    assert_eq!(ratatui::text::Span::raw(&cjk).width(), 16);
    assert_eq!(ratatui::text::Span::raw(&emoji).width(), 16);
}

/// The route line sizes the model's room from the provider's cells, so a CJK
/// provider never pushes the row past the pane.
#[test]
fn a_shunt_route_line_budgets_provider_and_model_by_cells() {
    let route = RouteView {
        position: 1,
        model: Some(w("opus")),
        provider: Some(w("日本語")),
        upstream_model: None,
        effort: None,
        service_tier: None,
    };
    let out = super::route_label(&route, 10);
    assert!(
        ratatui::text::Span::raw(&out).width() <= 10,
        "route overran: {out:?}"
    );
}

/// A wide (CJK) delegate profile name fits its column whole at a wide pane:
/// `name_w` and `key_cell` both budget cells, so the cell `truncate` no longer
/// cuts it though room remains.
#[test]
fn a_delegate_with_a_wide_name_keeps_it_whole() {
    use crate::mcp::jobs::JobPhase;
    let _home = crate::testutil::HomeSandbox::new();
    let cell = super::DelegateCells {
        state: JobPhase::Running,
        profile: "日本語".to_string(),
        facts: vec!["3m".to_string()],
        cwd: None,
    };
    let (lines, _, _) = super::delegate_job_lines(&[cell], None, false, 80);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "  ● running   日本語  3m");
}

/// The `stays` action sizes its path room from the reason's cells, so a CJK
/// reason the contract keeps whole is not cell-truncated.
#[test]
fn a_stays_action_keeps_a_cjk_reason_whole() {
    let spans = super::shunt_action_spans("stays  /home/u/repos/app  日本語", 30);
    assert_eq!(spans[3].content, "日本語");
}

// ── E5 width: the token rows budget their names in display cells ───────────

/// The `CLIENT TOKENS` group's content rows on the single-pane (stacked)
/// layout at terminal `width`: from the eyebrow through `+ add token`, each
/// row's border and one-cell pane padding dropped, trailing blanks trimmed.
/// The content width is `width - 4` (two borders, two padding cells).
fn stacked_token_rows(app: &App, width: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(width, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app, &[])).unwrap();
    let rows: Vec<String> = crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .map(|r| {
            let r = r.strip_prefix('│').unwrap_or(&r);
            let r = r.strip_suffix('│').unwrap_or(r);
            let r = r.strip_prefix(' ').unwrap_or(r);
            r.trim_end().to_string()
        })
        .collect();
    let start = rows
        .iter()
        .position(|r| r == "CLIENT TOKENS")
        .unwrap_or_else(|| panic!("the CLIENT TOKENS group:\n{}", rows.join("\n")));
    let end = rows[start..]
        .iter()
        .position(|r| r.contains("+ add token"))
        .unwrap_or_else(|| panic!("the + add token row:\n{}", rows.join("\n")));
    rows[start..=start + end].to_vec()
}

fn tokens_app(tokens: crate::tui::app::ShuntTokens) -> App {
    config_view_app_with_tokens(base_view(), std::collections::HashSet::new(), tokens)
}

/// At the 34-column floor (content 30 cells) a stored token's name keeps 18
/// cells (30 less the caret gutter, the two-space gap and the 8-cell chip) and
/// ends in `…` past them, so the ` remove ` chip renders whole; a CJK name
/// budgets two cells per glyph, never one per char.
#[test]
fn a_long_token_name_clamps_by_cells_and_keeps_the_remove_chip_whole() {
    use crate::tui::app::ShuntTokens;
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app(ShuntTokens {
        stored: vec![
            "first.last@company.example".into(),
            "日本語のプロファイル名前".into(),
            "bob".into(),
        ],
        ..ShuntTokens::default()
    });
    assert_eq!(
        stacked_token_rows(&app, 34),
        vec![
            "CLIENT TOKENS",
            "  first.last@compan…   remove",
            "  日 本 語 の プ ロ フ ァ …   remove",
            "  bob   remove",
            "  + add token",
        ]
    );
}

/// The env file's and the daemon env's client names clamp the same way, each
/// keeping room for its source word (`env file` 8 cells, `daemon env` 10).
#[test]
fn a_long_unmanaged_client_name_clamps_by_cells_and_keeps_its_source_word() {
    use crate::tui::app::{ShuntTokenSource, ShuntTokens};
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app(ShuntTokens {
        unmanaged: vec![
            (
                "another.client@company.example".into(),
                ShuntTokenSource::EnvFile,
            ),
            (
                "daemon-client-with-a-long-name".into(),
                ShuntTokenSource::DaemonEnv,
            ),
        ],
        ..ShuntTokens::default()
    });
    assert_eq!(
        stacked_token_rows(&app, 34),
        vec![
            "CLIENT TOKENS",
            "  another.client@co…  env file",
            "  daemon-client-w…  daemon env",
            "  + add token",
        ]
    );
}

/// An unparsed variable's name clamps so `not parsed` stays whole.
#[test]
fn a_long_unparsed_variable_clamps_by_cells_and_keeps_not_parsed() {
    use crate::tui::app::{ShuntTokenSource, ShuntTokens};
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app(ShuntTokens {
        unparsed: Some((
            "SHUNT_CLIENT_TOKENS_FOR_A_LONG_VARIABLE".into(),
            ShuntTokenSource::EnvFile,
        )),
        ..ShuntTokens::default()
    });
    assert_eq!(
        stacked_token_rows(&app, 34),
        vec![
            "CLIENT TOKENS",
            "  SHUNT_CLIENT_TO…  not parsed",
            "  + add token",
        ]
    );
}

/// Below the floor, at 18 columns (content 14 cells), the name budget beside
/// the chip falls under 4 cells: the name takes the row past the caret gutter
/// (12 cells, `…` past them) and the chip, source word or `not parsed` drops
/// to its own line under the gutter, whatever the name's own length.
#[test]
fn a_name_budget_under_four_cells_drops_the_chip_to_its_own_line() {
    use crate::tui::app::{ShuntTokenSource, ShuntTokens};
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app(ShuntTokens {
        stored: vec!["first.last@company.example".into(), "bob".into()],
        unmanaged: vec![(
            "another.client@company.example".into(),
            ShuntTokenSource::EnvFile,
        )],
        ..ShuntTokens::default()
    });
    assert_eq!(
        stacked_token_rows(&app, 18),
        vec![
            "CLIENT TOKENS",
            "  first.last@…",
            "   remove",
            "  bob",
            "   remove",
            "  another.cli…",
            "  env file",
            "  + add token",
        ]
    );
    let app = tokens_app(ShuntTokens {
        unparsed: Some(("SHUNT_CLIENT_TOKENS".into(), ShuntTokenSource::EnvFile)),
        ..ShuntTokens::default()
    });
    assert_eq!(
        stacked_token_rows(&app, 18),
        vec![
            "CLIENT TOKENS",
            "  SHUNT_CLIEN…",
            "  not parsed",
            "  + add token",
        ]
    );
}

/// The focused token row in the dropped shape keeps both its lines in place:
/// the caret on the name line, the chip on the next, and the rows after it
/// where the blurred render put them (the focused render replaces lines by
/// index, so a line count that changed with focus would overwrite a neighbour).
#[test]
fn a_focused_dropped_token_row_keeps_its_line_count() {
    use crate::tui::app::{ServicesFocus, ShuntFocusLine, ShuntTokens};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = tokens_app(ShuntTokens {
        stored: vec!["first.last@company.example".into(), "bob".into()],
        ..ShuntTokens::default()
    });
    stacked_token_rows(&app, 18);
    app.services.focus = ServicesFocus::Detail;
    app.services.set_shunt_focus(Some(ShuntFocusLine::Token(
        "first.last@company.example".into(),
    )));
    assert_eq!(
        stacked_token_rows(&app, 18),
        vec![
            "CLIENT TOKENS",
            "❯ first.last@…",
            "   remove",
            "  bob",
            "   remove",
            "  + add token",
        ]
    );
}

/// The read-only bind row budgets its trailing `set by …` suffix against the
/// value's display cells: a CJK value (8 two-cell glyphs + `:3001`, 21 cells,
/// 13 chars) leaves the suffix 9 cells of the 32-cell value room, so it ends
/// in `…` inside the pane instead of running off its edge.
#[test]
fn the_bind_suffix_budgets_a_wide_value_by_cells() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.server.bind = Some(w("日本語のホスト名:3001"));
    view.server.bind_override = BindOverride::Set("SHUNT_SERVER__BIND".to_string());
    let rows = render_config_section(view, std::collections::HashSet::new(), 80);
    assert_eq!(
        rows[1],
        "  bind              日 本 語 の ホ ス ト 名 :3001  set by S…",
        "the bind row:\n{}",
        rows.join("\n")
    );
}

/// The drop threshold itself: at 20 columns (content 16) a token's budget is
/// exactly 4 cells and stays inline, as does an `env file` name (budget 4),
/// while a `daemon env` name (budget 2) drops; at 19 columns (content 15)
/// every budget is under 4 and every trailer drops. Each row drops on its own
/// trailer's width.
#[test]
fn the_drop_threshold_is_a_budget_of_four_cells() {
    use crate::tui::app::{ShuntTokenSource, ShuntTokens};
    let _home = crate::testutil::HomeSandbox::new();
    let tokens = || ShuntTokens {
        stored: vec!["bob".into()],
        unmanaged: vec![
            ("ef".into(), ShuntTokenSource::EnvFile),
            ("dc".into(), ShuntTokenSource::DaemonEnv),
        ],
        ..ShuntTokens::default()
    };
    assert_eq!(
        stacked_token_rows(&tokens_app(tokens()), 20),
        vec![
            "CLIENT TOKENS",
            "  bob   remove",
            "  ef  env file",
            "  dc",
            "  daemon env",
            "  + add token",
        ]
    );
    assert_eq!(
        stacked_token_rows(&tokens_app(tokens()), 19),
        vec![
            "CLIENT TOKENS",
            "  bob",
            "   remove",
            "  ef",
            "  env file",
            "  dc",
            "  daemon env",
            "  + add token",
        ]
    );
}

/// The focused token row's dropped chip line carries the focused chip: its
/// `remove` cells sit on the `DANGER` inverse fill, while a blurred token's
/// dropped chip does not.
#[test]
fn a_focused_dropped_chip_renders_as_the_focused_inverse_block() {
    use crate::tui::app::{ServicesFocus, ShuntFocusLine, ShuntTokens};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = tokens_app(ShuntTokens {
        stored: vec!["alice".into(), "bob".into()],
        ..ShuntTokens::default()
    });
    stacked_token_rows(&app, 18);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Token("alice".into())));
    let mut term = Terminal::new(TestBackend::new(18, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
    let buffer = term.backend().buffer();
    let rows = crate::testutil::buffer_rows(buffer);
    let caret = rows
        .iter()
        .position(|r| r.contains("❯ alice"))
        .unwrap_or_else(|| panic!("the focused alice row:\n{}", rows.join("\n")));
    let bob = rows
        .iter()
        .position(|r| r.contains("  bob"))
        .unwrap_or_else(|| panic!("the bob row:\n{}", rows.join("\n")));
    // The chip's `remove` letters on the line under each name.
    let chip_bgs = |y: usize| -> Vec<ratatui::style::Color> {
        let text = &rows[y];
        let start = text.find("remove").expect("a remove chip");
        let x0 = text[..start].chars().count() as u16;
        (x0..x0 + 6).map(|x| buffer[(x, y as u16)].bg).collect()
    };
    assert!(
        chip_bgs(caret + 1)
            .iter()
            .all(|bg| *bg == crate::tui::theme::danger_color()),
        "the focused token's dropped chip is the DANGER inverse block:\n{}",
        rows.join("\n")
    );
    assert!(
        chip_bgs(bob + 1)
            .iter()
            .all(|bg| *bg != crate::tui::theme::danger_color()),
        "a blurred token's dropped chip is not filled:\n{}",
        rows.join("\n")
    );
}

/// The read-only bind row shows its `set by …` suffix only while at least 4
/// cells are left for it: with 1 or 3 cells left (an ASCII value 29 or 27
/// cells wide in the 32-cell value room at 80 columns) the row ends at the
/// value, never on a lone `…`; with 4 left it reads `set…`.
#[test]
fn the_bind_suffix_shows_only_with_four_cells_left() {
    let _home = crate::testutil::HomeSandbox::new();
    for (value_w, suffix) in [(29usize, ""), (27, ""), (26, "  set…")] {
        let value = format!("{}:1", "h".repeat(value_w - 2));
        let mut view = base_view();
        view.server.bind = Some(w(&value));
        view.server.bind_override = BindOverride::Set("SHUNT_SERVER__BIND".to_string());
        let rows = render_config_section(view, std::collections::HashSet::new(), 80);
        assert_eq!(
            rows[1],
            format!("  bind              {value}{suffix}"),
            "a {value_w}-cell value"
        );
    }
}

/// The bind suffix budget counts what the buffer draws: a lam-alef pair is
/// one glyph to a whole-string width but two cells in the buffer, so a value of
/// four pairs + `:1` (10 cells) leaves the suffix 20 of the 32-cell room and it
/// ends in `…` inside the pane.
#[test]
fn the_bind_suffix_budgets_a_lam_alef_value_by_drawn_cells() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.server.bind = Some(w(&format!("{}:1", "لا".repeat(4))));
    view.server.bind_override = BindOverride::Set("SHUNT_SERVER__BIND".to_string());
    let rows = render_config_section(view, std::collections::HashSet::new(), 80);
    assert_eq!(
        rows[1],
        "  bind              لالالالا:1  set by SHUNT_SERVER…",
        "the bind row:\n{}",
        rows.join("\n")
    );
}

/// S10: a typed shunt value wider than its field scrolls inside its row: End
/// keeps the caret inside the pane with the last grapheme (a two-cell rocket)
/// whole before it, and one ← lands on the rocket without moving any drawn
/// cell of the row.
#[test]
fn an_over_wide_typed_shunt_value_scrolls_inside_its_row() {
    use crate::testutil::{draw_caret, right_border, symbol_at};
    use crate::tui::app::{
        InputState, ServicesFocus, ShuntFieldEdit, ShuntFieldEditState, ShuntFieldKey,
        ShuntFocusLine, UpstreamFieldKind,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("a")),
        provider: None,
        kind: None,
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    }]);
    let key = ShuntFieldKey::Upstream {
        name: "a".into(),
        occurrence: 0,
        field: UpstreamFieldKind::Effort,
    };
    let mut app = config_view_app(view, expanded);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Field(key.clone())));
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key,
        edit: ShuntFieldEdit::Typed {
            input: InputState::new(&format!("{}🚀", "x".repeat(150))),
            saved: None,
        },
        refusal: None,
    });
    let step_left =
        |app: &mut App| match app.services.shunt_field_edit.as_mut().map(|s| &mut s.edit) {
            Some(ShuntFieldEdit::Typed { input, .. }) => input.left(),
            _ => panic!("the typed editor is open"),
        };
    let mut term = Terminal::new(TestBackend::new(W, 60)).unwrap();

    let (end, buf) = draw_caret(&mut term, |f| super::draw(f, f.area(), &app, &[]));
    let border = right_border(&buf, end.y);
    assert!(
        end.x + 2 <= border,
        "End stays inside the field ({end:?}, border {border})"
    );
    assert_eq!(
        symbol_at(&buf, end.x - 2, end.y),
        "🚀",
        "the rocket draws whole before End"
    );
    let end_row = crate::testutil::buffer_rows(&buf)[end.y as usize].clone();

    step_left(&mut app);
    let (left, buf) = draw_caret(&mut term, |f| super::draw(f, f.area(), &app, &[]));
    assert_eq!(
        left,
        ratatui::layout::Position::new(end.x - 2, end.y),
        "one left steps over the rocket's two cells"
    );
    assert_eq!(
        symbol_at(&buf, left.x, left.y),
        "🚀",
        "the caret sits on it"
    );
    assert_eq!(
        crate::testutil::buffer_rows(&buf)[left.y as usize],
        end_row,
        "a move inside the window leaves the row alone"
    );
}

/// One herdr-options frame at 100 x 60, tall enough that the form never
/// scrolls, so frames compare row for row.
fn herdr_frame(app: &App) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(W, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app, &[])).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
}

/// S10 (cloudy's range rule, 2026-10-14): `tag refresh` has no hint at rest;
/// typing adds its one-line range under the row and the rows below move down
/// by exactly that line, with no blank line held at rest.
#[test]
fn the_herdr_tag_refresh_editor_adds_its_range_line_below_the_row() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(healthy_config());
    app.services.herdr_options_cursor = 2;
    let resting = herdr_frame(&app);
    app.services.herdr_tag_draft = Some(crate::tui::app::SeededInput::new("5"));
    let editing = herdr_frame(&app);
    let at = |rows: &[String], label: &str| {
        rows.iter()
            .position(|r| r.contains(label))
            .unwrap_or_else(|| panic!("{label} renders:\n{}", rows.join("\n")))
    };
    let field = at(&resting, "tag refresh");
    let below = at(&resting, "border label");
    assert_eq!(below, field + 1, "no line sits under the row at rest");
    assert!(
        editing[field + 1].contains("└ min is 1 s"),
        "the range line sits under the edit line:\n{}",
        editing.join("\n")
    );
    crate::testutil::assert_rows_below_shift(&resting, &editing, field, below, 1, 3, "herdr");
}

/// S10: a tag-refresh buffer wider than its field scrolls inside the row: End
/// keeps the caret inside the pane right after the last digit, one ← moves it
/// one cell and leaves the row alone, Home shows the first digit again.
#[test]
fn an_over_wide_tag_refresh_buffer_scrolls_inside_its_row() {
    use crate::testutil::{draw_caret, right_border, symbol_at};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(healthy_config());
    app.services.herdr_options_cursor = 2;
    app.services.herdr_tag_draft =
        Some(crate::tui::app::SeededInput::new(&"1234567890".repeat(12)));
    fn draft(app: &mut App) -> &mut crate::tui::app::InputState {
        &mut app
            .services
            .herdr_tag_draft
            .as_mut()
            .expect("the editor is open")
            .input
    }
    let mut term = Terminal::new(TestBackend::new(W, 60)).unwrap();

    let (end, buf) = draw_caret(&mut term, |f| super::draw(f, f.area(), &app, &[]));
    let border = right_border(&buf, end.y);
    assert!(
        end.x + 2 <= border,
        "End stays inside the field ({end:?}, border {border})"
    );
    assert_eq!(
        symbol_at(&buf, end.x - 1, end.y),
        "0",
        "End follows the last digit"
    );
    let end_row = crate::testutil::buffer_rows(&buf)[end.y as usize].clone();

    draft(&mut app).left();
    let (left, buf) = draw_caret(&mut term, |f| super::draw(f, f.area(), &app, &[]));
    assert_eq!(
        left,
        ratatui::layout::Position::new(end.x - 1, end.y),
        "one left moves one cell"
    );
    assert_eq!(
        crate::testutil::buffer_rows(&buf)[left.y as usize],
        end_row,
        "a move inside the window leaves the row alone"
    );

    draft(&mut app).home();
    let (home, buf) = draw_caret(&mut term, |f| super::draw(f, f.area(), &app, &[]));
    assert_eq!(home.y, end.y, "Home stays on the field row");
    assert_eq!(
        symbol_at(&buf, home.x, home.y),
        "1",
        "Home sits on the first digit"
    );
}

/// S10 (cloudy's ruling, 2026-10-14: let rows move): opening the custom-value
/// editor on a wrapped cycle row (`provider`'s run takes three lines at 100
/// columns) collapses it to the one edit line, and the rows below move up by
/// exactly two, with no blank or tinted line held in their place.
#[test]
fn a_typed_editor_on_a_wrapped_cycle_row_moves_the_rows_below_up() {
    use crate::tui::app::{
        InputState, ServicesFocus, ShuntFieldEdit, ShuntFieldEditState, ShuntFieldKey,
        ShuntFocusLine, UpstreamFieldKind,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("a")),
        provider: Some(w("cursor")),
        kind: None,
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    }]);
    let key = ShuntFieldKey::Upstream {
        name: "a".into(),
        occurrence: 0,
        field: UpstreamFieldKind::Provider,
    };
    let mut app = config_view_app(view, expanded);
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Field(key.clone())));
    let frame = |app: &App| {
        let mut term = Terminal::new(TestBackend::new(W, 60)).unwrap();
        term.draw(|f| super::draw(f, f.area(), app, &[])).unwrap();
        crate::testutil::buffer_rows(term.backend().buffer())
    };
    let resting = frame(&app);
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key,
        edit: ShuntFieldEdit::Typed {
            input: InputState::new("cursor"),
            saved: Some("cursor".to_string()),
        },
        refusal: None,
    });
    let editing = frame(&app);
    let at = |rows: &[String], label: &str| {
        rows.iter()
            .position(|r| r.contains(label))
            .unwrap_or_else(|| panic!("{label} renders:\n{}", rows.join("\n")))
    };
    let field = at(&resting, "❯   provider");
    let below = at(&resting, "kind");
    assert_eq!(below, field + 3, "the run wraps:\n{}", resting.join("\n"));
    crate::testutil::assert_rows_below_shift(&resting, &editing, field, below, -2, 3, "shunt");
}

/// S10: a typed shunt field keeps its caret inside the pane on a narrow
/// terminal: the value column clamps to `MIN_VALUE_W` below the pane width.
#[test]
fn a_typed_shunt_field_keeps_its_caret_inside_on_a_narrow_pane() {
    use crate::testutil::{draw_caret, right_border, symbol_at};
    use crate::tui::app::{
        InputState, ServicesFocus, ShuntFieldEdit, ShuntFieldEditState, ShuntFieldKey,
        ShuntFocusLine, UpstreamFieldKind,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.upstreams = vec![UpstreamView {
        name: Some(w("a")),
        provider: None,
        kind: None,
        base_url: None,
        auth: None,
        effort: None,
        service_tier: None,
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Upstream {
        name: "a".into(),
        occurrence: 0,
    }]);
    let key = ShuntFieldKey::Upstream {
        name: "a".into(),
        occurrence: 0,
        field: UpstreamFieldKind::Effort,
    };
    for w in [16u16, 18, 20, 22, 28, 34] {
        let mut app = config_view_app(view.clone(), expanded.clone());
        app.services.focus = ServicesFocus::Detail;
        app.services
            .set_shunt_focus(Some(ShuntFocusLine::Field(key.clone())));
        app.services.shunt_field_edit = Some(ShuntFieldEditState {
            key: key.clone(),
            edit: ShuntFieldEdit::Typed {
                input: InputState::new("hi"),
                saved: None,
            },
            refusal: None,
        });
        let mut term = Terminal::new(TestBackend::new(w, 100)).unwrap();
        let (end, buf) = draw_caret(&mut term, |f| super::draw(f, f.area(), &app, &[]));
        let border = right_border(&buf, end.y);
        assert!(
            end.x + 2 <= border,
            "[{w}] End stays inside the field ({end:?}, border {border})"
        );
        assert_eq!(
            symbol_at(&buf, end.x - 1, end.y),
            "i",
            "[{w}] End follows the last typed char"
        );
    }
}

/// S10: a typed herdr `tag refresh` field keeps its caret inside the pane on
/// a narrow terminal: the label column clamps and the label truncates below
/// `MIN_VALUE_W` + the ` s` unit.
#[test]
fn a_typed_herdr_tag_refresh_keeps_its_caret_inside_on_a_narrow_pane() {
    use crate::testutil::{draw_caret, right_border, symbol_at};
    let _home = crate::testutil::HomeSandbox::new();
    for w in [16u16, 18, 20, 22, 28, 34] {
        let mut app = herdr_options_app(healthy_config());
        app.services.herdr_options_cursor = 2;
        app.services.herdr_tag_draft = Some(crate::tui::app::SeededInput::new("5"));
        let mut term = Terminal::new(TestBackend::new(w, 100)).unwrap();
        let (end, buf) = draw_caret(&mut term, |f| super::draw(f, f.area(), &app, &[]));
        let border = right_border(&buf, end.y);
        assert!(
            end.x + 2 <= border,
            "[{w}] End stays inside the field ({end:?}, border {border})"
        );
        assert_eq!(
            symbol_at(&buf, end.x - 1, end.y),
            "5",
            "[{w}] End follows the last digit"
        );
    }
}

/// S10: the resting herdr rows keep their r2 bytes at terminal 28-29: only the
/// tag-refresh editor budgets its ` s` unit, so the `delegate row text` label
/// stays whole at the narrow-stack layout's interior 26.
#[test]
fn the_resting_herdr_rows_keep_their_full_labels_at_28_cols() {
    let _home = crate::testutil::HomeSandbox::new();
    for w in [28u16, 29] {
        let mut app = herdr_options_app(healthy_config());
        app.services.herdr_options_cursor = 5;
        let mut term = Terminal::new(TestBackend::new(w, 100)).unwrap();
        term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let row = rows
            .iter()
            .find(|r| r.contains("delegate row text"))
            .unwrap_or_else(|| panic!("[{w}] the delegate row renders:\n{}", rows.join("\n")));
        assert!(
            !row.contains("delegate row t…"),
            "[{w}] the label truncates:\n{row}"
        );
        let label_at = row
            .find("delegate row text")
            .expect("the full label renders");
        assert_eq!(
            &row[label_at + "delegate row text".len()..label_at + "delegate row text".len() + 2],
            "  ",
            "[{w}] the value opens at the label's 2-cell gap (r2 bytes):\n{row}"
        );
    }
}

/// S10: the typed shunt form keeps the resting label gap and escapes the label
/// like the resting `config_key`: the value opens at the lead with the 2-cell
/// gutter at every width, and a control char in a provider-name label renders
/// as its `\u{…}` form, never raw.
#[test]
fn a_typed_shunt_field_keeps_the_label_gap_and_escapes_it() {
    use crate::tui::app::{
        InputState, ModelFieldKind, ServicesFocus, ShuntFieldEdit, ShuntFieldEditState,
        ShuntFieldKey, ShuntFocusLine,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let mut view = base_view();
    view.models = vec![ModelView {
        id: Some(w("m")),
        display_name: None,
        upstream_model: Some(vec![("a\tb".to_string(), w("claude-opus"))]),
    }];
    let expanded = std::collections::HashSet::from([ShuntEntryKey::Model {
        id: "m".into(),
        occurrence: 0,
    }]);
    let key = ShuntFieldKey::Model {
        id: "m".into(),
        occurrence: 0,
        field: ModelFieldKind::UpstreamModel("a\tb".to_string()),
    };
    for w in [16u16, 28, 45] {
        let mut app = config_view_app(view.clone(), expanded.clone());
        app.services.focus = ServicesFocus::Detail;
        app.services
            .set_shunt_focus(Some(ShuntFocusLine::Field(key.clone())));
        app.services.shunt_field_edit = Some(ShuntFieldEditState {
            key: key.clone(),
            edit: ShuntFieldEdit::Typed {
                input: InputState::new("hi"),
                saved: None,
            },
            refusal: None,
        });
        let mut term = Terminal::new(TestBackend::new(w, 60)).unwrap();
        term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let row = rows
            .iter()
            .find(|r| r.contains('✎'))
            .unwrap_or_else(|| panic!("[{w}] the typed row renders:\n{}", rows.join("\n")));
        let at = row.find("hi").unwrap_or(usize::MAX);
        assert_eq!(
            &row[at.saturating_sub(2)..at],
            "  ",
            "[{w}] a 2-cell gap opens the value:\n{row}"
        );
        assert!(!row.contains('\t'), "[{w}] a raw tab renders:\n{row}");
        if w >= 28 {
            assert!(
                row.contains("a\\tb"),
                "[{w}] the label escapes to its `\\t` form:\n{row}"
            );
        }
    }
}

/// S10: the RESTING `tag refresh` row keeps its full label at 16-24 cols (the
/// ` s` budget applies only while its editor is open); the `5s` value clips at
/// the pane edge, never the label.
#[test]
fn the_resting_tag_refresh_row_keeps_its_full_label_on_narrow_panes() {
    let _home = crate::testutil::HomeSandbox::new();
    for w in [16u16, 20, 24, 28] {
        let mut app = herdr_options_app(healthy_config());
        app.services.herdr_options_cursor = 2;
        assert!(app.services.herdr_tag_draft.is_none(), "the row rests");
        let mut term = Terminal::new(TestBackend::new(w, 100)).unwrap();
        term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let expect = if w == 16 { "tag refres" } else { "tag refresh" };
        let row = rows.iter().find(|r| r.contains(expect)).unwrap_or_else(|| {
            panic!(
                "[{w}] the resting label renders whole:\n{}",
                rows.join("\n")
            )
        });
        assert!(
            !row.contains("t…"),
            "[{w}] the resting label never truncates:\n{row}"
        );
        if w >= 20 {
            let label_at = row.find("tag refresh").expect("the full label renders");
            assert_eq!(
                &row[label_at + "tag refresh".len()..label_at + "tag refresh".len() + 2],
                "  ",
                "[{w}] the value opens at the label's 2-cell gap:\n{row}"
            );
        }
    }
}

// ── tps: the delegate row's `S t/s` fact ─────────────────────────────────

/// A running row's facts gain its live `S t/s`, a finished row's its own
/// average; both ride the row as an extra fact the builder is passed.
#[test]
fn a_delegate_row_carries_its_speed_fact_when_the_figure_is_passed_in() {
    use crate::mcp::jobs::JobPhase;
    use ratatui::text::Line;

    let line_text = |line: &Line| {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    };
    let running = super::DelegateCells {
        state: JobPhase::Running,
        profile: "acct".to_string(),
        facts: vec!["elapsed 5s".to_string(), super::speed_fact(92.4)],
        cwd: None,
    };
    assert_eq!(
        line_text(&super::delegate_row_line(&running, 4, 100, false, false)),
        "  ● running   acct  elapsed 5s · 92 t/s"
    );
    let done = super::DelegateCells {
        state: JobPhase::Done,
        profile: "acct".to_string(),
        facts: vec!["finished just now".to_string(), super::speed_fact(88.0)],
        cwd: None,
    };
    assert_eq!(
        line_text(&super::delegate_row_line(&done, 4, 100, false, false)),
        "  ● done      acct  finished just now · 88 t/s"
    );
}

/// A finished job's row reads its average off the App cache the tick path
/// fills — the render never walks the store — and the cache evicts ids that
/// left the delegate list.
#[test]
fn a_finished_jobs_speed_reads_the_cache_and_evicts_gone_ids() {
    let _home = crate::testutil::HomeSandbox::new();

    // The formula: output ÷ generation seconds, None without generation time.
    assert_eq!(
        crate::tui::app::day_average(crate::tps::DaySum {
            output: 900,
            gen_ms: 10_000,
            ..Default::default()
        }),
        Some(90.0)
    );
    assert_eq!(
        crate::tui::app::day_average(crate::tps::DaySum {
            output: 900,
            gen_ms: 0,
            ..Default::default()
        }),
        None
    );

    // A finished record listed through the store's own reader.
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-done-0.json"),
        serde_json::to_vec(&serde_json::json!({
            "job_id": "d-done-0",
            "profile": "acct",
            "state": "done",
            "started_at": NOW - 1_800_000,
            "done_at": NOW - 900_000,
            "session_id": "sess-1",
        }))
        .unwrap(),
    )
    .unwrap();
    let mut app = app_with_delegates(jobs::list(NOW));
    let done = &app.services.delegates[0];

    // Uncached → no fact (the sandbox store has no transcript to resolve).
    assert_eq!(super::delegate_speed_fact(done, &app), None);

    // A resolved figure in the cache renders through the row, and a second ask
    // still returns it — the render never re-resolves.
    app.delegate_speed_cache.insert("d-done-0".into(), 90.0);
    assert_eq!(
        super::delegate_speed_fact(done, &app),
        Some("90 t/s".to_string())
    );
    assert_eq!(
        super::delegate_speed_fact(done, &app),
        Some("90 t/s".to_string())
    );

    // The tick pass evicts a cached id that left the delegate list.
    app.delegate_speed_cache.insert("stale-id".into(), 42.0);
    crate::tui::app::resolve_finished_speeds(&mut app);
    assert!(
        !app.delegate_speed_cache.contains_key("stale-id"),
        "a job gone from the list is evicted"
    );
}

/// A running job's live speed reads its OWN session, never the account's
/// figure: a snapshot where the two differ still yields the session's value.
#[test]
fn a_running_jobs_speed_reads_its_own_session_not_the_account() {
    let _home = crate::testutil::HomeSandbox::new();
    jobs::write_heartbeat_with_session(
        &running_spec("d-run-0", "acct", NOW - 5_000, RecordKind::Collectable),
        NOW - 1_000,
        "working",
        Some("sess-1"),
    )
    .unwrap();
    let mut app = app_with_delegates(jobs::list(NOW));

    let stamp = chrono::DateTime::from_timestamp_millis(
        i64::try_from(crate::usage::now_ms() - 1_000).unwrap(),
    )
    .unwrap()
    .format("%Y-%m-%dT%H:%M:%S%.3f+00:00")
    .to_string();
    let figures = |speed: f64| crate::tps::Figures {
        live: crate::tps::Figure {
            rate: 1.0,
            speed: Some(speed),
        },
        ..Default::default()
    };
    let mut snap = crate::tps::Snapshot {
        version: crate::tps::CACHE_VERSION,
        generated_at: stamp,
        accounts: std::collections::BTreeMap::from([("acct".to_string(), figures(99.0))]),
        sessions: std::collections::BTreeMap::from([(
            "sess-1".to_string(),
            crate::tps::SessionFigures {
                account: Some("acct".to_string()),
                figures: figures(42.0),
            },
        )]),
        ..Default::default()
    };
    snap.total = figures(42.0);
    app.set_tps(Some(snap));

    let job = &app.services.delegates[0];
    assert_eq!(
        job.phase(),
        super::JobPhase::Running,
        "the fixture is running"
    );
    assert_eq!(
        super::delegate_speed_fact(job, &app),
        Some("42 t/s".to_string()),
        "the session's speed, not the account's 99"
    );
}

/// The finished-speed resolver walks the transcript store once per pass for
/// every pending id, and backs a still-unresolved job off (2 s doubling) so the
/// store is not re-walked for it every second.
#[test]
fn the_finished_speed_resolver_walks_once_and_backs_off() {
    let _home = crate::testutil::HomeSandbox::new();
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    for (id, sid) in [("d-a", "sess-a"), ("d-b", "sess-b")] {
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_vec(&serde_json::json!({
                "job_id": id,
                "profile": "acct",
                "state": "done",
                "started_at": NOW - 1_800_000,
                "done_at": NOW - 900_000,
                "session_id": sid,
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let mut app = app_with_delegates(jobs::list(NOW));

    crate::tui::app::resolve_finished_speeds(&mut app);
    assert_eq!(
        app.delegate_speed_walks, 1,
        "one walk covers both pending ids"
    );
    assert_eq!(app.delegate_speed_retry.len(), 2);
    for retry in app.delegate_speed_retry.values() {
        assert_eq!(retry.backoff_ms, 2_000, "first failure backs off 2 s");
    }

    crate::tui::app::resolve_finished_speeds(&mut app);
    assert_eq!(
        app.delegate_speed_walks, 1,
        "no second walk while both jobs are backed off"
    );

    for retry in app.delegate_speed_retry.values_mut() {
        retry.next_ms = 0;
    }
    crate::tui::app::resolve_finished_speeds(&mut app);
    assert_eq!(app.delegate_speed_walks, 2);
    for retry in app.delegate_speed_retry.values() {
        assert_eq!(retry.backoff_ms, 4_000, "the back-off doubles");
    }
}

/// A finished delegate whose transcript sits in the global store resolves to
/// its own average through the producer's `transcript_speed`: the fixture's
/// one 1222-token response over 13.602 s reads 89.84 t/s.
#[test]
fn a_finished_jobs_speed_resolves_from_its_transcript() {
    let home = crate::testutil::HomeSandbox::new();
    let sid = "0c1f6a52-7d4e-4b0a-9a51-3f2b8e6c1d01";
    let store = home.home().join(".claude/projects/-home-u-proj");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join(format!("{sid}.jsonl")),
        include_str!("../fixtures/tps-one-response.jsonl"),
    )
    .unwrap();
    let dir = jobs::jobs_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("d-fix.json"),
        serde_json::to_vec(&serde_json::json!({
            "job_id": "d-fix",
            "profile": "acct",
            "state": "done",
            "started_at": NOW - 1_800_000,
            "done_at": NOW - 900_000,
            "session_id": sid,
        }))
        .unwrap(),
    )
    .unwrap();
    let mut app = app_with_delegates(jobs::list(NOW));
    // Seed a stale back-off for this job, so the `is_empty` check below proves
    // the resolved arm clears it rather than trivially passing on an empty map.
    app.delegate_speed_retry.insert(
        "d-fix".into(),
        crate::tui::app::FinishedRetry {
            next_ms: 0,
            backoff_ms: 0,
        },
    );

    crate::tui::app::resolve_finished_speeds(&mut app);

    let speed = app.delegate_speed_cache.get("d-fix").copied();
    assert!(
        speed.is_some_and(|s| (s - 1222.0 / 13.602).abs() < 1e-3),
        "the job's own average: {speed:?}"
    );
    assert!(
        app.delegate_speed_retry.is_empty(),
        "a resolved job keeps no back-off"
    );
}

/// A shunt config field's caret that lands inside a drawn toast hides the
/// native cursor this frame, and it hides under an open modal too. Three
/// stacked DANGER toasts cover the bind field's caret column at 45x14.
#[test]
fn a_toast_over_a_shunt_field_caret_hides_the_native_cursor() {
    use crate::tui::app::{
        InputState, Modal, ServicesFocus, ShuntFieldEdit, ShuntFieldEditState, ShuntFieldKey,
        ShuntFocusLine, ToastKind,
    };
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);

    let mut app = config_view_app(base_view(), std::collections::HashSet::new());
    app.services.focus = ServicesFocus::Detail;
    app.services
        .set_shunt_focus(Some(ShuntFocusLine::Field(ShuntFieldKey::Bind)));
    app.services.shunt_field_edit = Some(ShuntFieldEditState {
        key: ShuntFieldKey::Bind,
        edit: ShuntFieldEdit::Typed {
            input: InputState::new("127.0.0.1:3001"),
            saved: None,
        },
        refusal: None,
    });
    // The bind row sits below the card header; scroll it to the top so its
    // caret is in view at 45x14.
    app.services.shunt_scroll.set(11);

    let mut term = Terminal::new(TestBackend::new(45, 14)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        term.backend().cursor_visible(),
        "the shunt caret shows at 45x14 with no toasts"
    );

    let wide = "x".repeat(53);
    for label in ["one", "two", "three"] {
        app.toast(ToastKind::Danger, format!("{label}\n{wide}\n{label} tail"));
    }

    let mut term = Terminal::new(TestBackend::new(45, 14)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        !term.backend().cursor_visible(),
        "[45x14] the shunt caret hides under a toast"
    );

    app.toasts.clear();
    app.open_modal(Modal::Help);
    let mut term = Terminal::new(TestBackend::new(45, 14)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        !term.backend().cursor_visible(),
        "[45x14] the shunt caret hides under an open modal"
    );
}

/// A herdr tag-refresh field's caret that lands inside a drawn toast hides the
/// native cursor this frame, and it hides under an open modal too. Three
/// stacked DANGER toasts cover the tag field's caret column at 45x14.
#[test]
fn a_toast_over_the_herdr_tag_field_caret_hides_the_native_cursor() {
    use crate::tui::app::{Modal, Tab, ToastKind};
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);

    let mut app = herdr_options_app(healthy_config());
    app.tab = Tab::Services;
    app.services.herdr_options_cursor = 2;
    app.services.herdr_tag_draft = Some(crate::tui::app::SeededInput::new("5"));

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        term.backend().cursor_visible(),
        "the herdr tag caret shows with no toasts"
    );

    let wide = "x".repeat(53);
    for label in ["one", "two", "three"] {
        app.toast(ToastKind::Danger, format!("{label}\n{wide}\n{label} tail"));
    }

    let mut term = Terminal::new(TestBackend::new(45, 14)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        !term.backend().cursor_visible(),
        "[45x14] the herdr tag caret hides under a toast"
    );

    app.toasts.clear();
    app.open_modal(Modal::Help);
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    assert!(
        !term.backend().cursor_visible(),
        "the herdr tag caret hides under an open modal"
    );
}

/// The descended delegates detail writes its page keys joined, `pgup/pgdn`,
/// never the multi-word `page up/down`.
#[test]
fn the_delegates_detail_writes_its_page_keys_joined() {
    use crate::tui::app::ServicesFocus;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_delegates(seed_walk_jobs(22));
    app.services.focus = ServicesFocus::Detail;
    render(&app);
    let hints = super::super::footer::services_hints(&app);
    assert!(
        hints.contains(&("pgup/pgdn", "read")),
        "the page keys read `pgup/pgdn`: {hints:?}"
    );
    assert!(
        !hints.iter().any(|(key, _)| key.contains("page")),
        "no hint spells `page up`: {hints:?}"
    );
}

/// The plugin card the recompute builds while a fresh daemon runs a replaced
/// binary: every row above the line renders exactly as the card without it
/// (the line never joins the `key: value` column), then a blank row, then the
/// line wrapped whole at every width, in the warning tone with its command
/// marked, closing the card.
#[test]
fn the_daemon_replaced_line_wraps_whole_under_an_unchanged_card() {
    assert_daemon_binary_line_wraps_whole(
        crate::plugin_host::ExeChange::Replaced,
        "daemon still on the old binary: plugin self-heal off until it restarts (clauth daemon --replace)",
    );
}

/// The same card while the daemon's binary was removed: the removal's own line,
/// laid out under the same contract.
#[test]
fn the_daemon_removed_line_wraps_whole_under_an_unchanged_card() {
    assert_daemon_binary_line_wraps_whole(
        crate::plugin_host::ExeChange::Removed,
        "daemon's binary was removed: plugin self-heal off until clauth is reinstalled and it restarts (clauth daemon --replace)",
    );
}

fn assert_daemon_binary_line_wraps_whole(change: crate::plugin_host::ExeChange, shown: &str) {
    use crate::tui::app::{ShuntRefresh, Tab, recompute_services_checks};
    const TALL: u16 = 40;
    let _home = crate::testutil::HomeSandbox::new();
    let head: String = shown.chars().take(20).collect();
    let compact: Vec<char> = shown.chars().filter(|c| *c != ' ').collect();
    let command_len = "clauthdaemon--replace".len();
    let command_at = compact.len() - command_len - 1;
    for wide in [80u16, 120, 160] {
        let frame = |changed: bool| {
            let mut app = App::new(AppConfig {
                state: AppState::default(),
                profiles: Vec::new(),
            });
            app.tab = Tab::Services;
            app.daemon_binary_change = changed.then_some(change);
            recompute_services_checks(&mut app, false, ShuntRefresh::Keep);
            app.services.cursor = app
                .services
                .checks
                .iter()
                .position(|c| c.label == "plugin")
                .expect("plugin row");
            let mut term = Terminal::new(TestBackend::new(wide, TALL)).unwrap();
            term.draw(|f| super::draw(f, f.area(), &app, &[])).unwrap();
            term.backend().buffer().clone()
        };
        let without = frame(false);
        let with = frame(true);
        let rows: Vec<Vec<char>> = crate::testutil::buffer_rows(&with)
            .iter()
            .map(|r| r.chars().collect())
            .collect();
        let w = wide as usize;
        let text = |row: &[char]| row.iter().collect::<String>();
        let first = rows
            .iter()
            .position(|r| text(r).contains(head.as_str()))
            .unwrap_or_else(|| panic!("{wide} cols: no line: {rows:#?}"));
        let col0 = text(&rows[first])
            .find(head.as_str())
            .map_or(0, |at| text(&rows[first])[..at].chars().count());
        let right = rows[first]
            .iter()
            .rposition(|c| *c == '│')
            .expect("the pane's right border");

        for (row, chars) in rows.iter().enumerate().take(first - 1) {
            for col in 0..w {
                let (a, b) = (
                    &with.content[row * w + col],
                    &without.content[row * w + col],
                );
                assert_eq!(
                    (a.symbol(), a.style()),
                    (b.symbol(), b.style()),
                    "{wide} cols, row {row}, column {col}: {:?}",
                    text(chars)
                );
            }
        }
        assert!(
            rows[first - 1][col0..right].iter().all(|c| *c == ' '),
            "{wide} cols: a blank row sets the line off: {:?}",
            text(&rows[first - 1])
        );

        let mut joined: Vec<String> = Vec::new();
        let mut cells: Vec<(char, ratatui::style::Color)> = Vec::new();
        for (row, chars) in rows.iter().enumerate().skip(first) {
            let piece = text(&chars[col0..right]);
            if piece.trim().is_empty() {
                break;
            }
            joined.push(piece.trim().to_string());
            for (col, ch) in chars.iter().enumerate().take(right).skip(col0) {
                let cell = &with.content[row * w + col];
                if cell.symbol() != " " {
                    cells.push((*ch, cell.fg));
                }
            }
        }
        assert_eq!(joined.join(" "), shown, "{wide} cols: the line wraps whole");
        assert_eq!(
            cells.len(),
            compact.len(),
            "{wide} cols: every glyph renders"
        );
        for (i, (ch, fg)) in cells.iter().enumerate() {
            let expected = if (command_at..command_at + command_len).contains(&i) {
                super::theme::accent_color()
            } else {
                super::theme::warning_color()
            };
            assert_eq!(
                (*ch, *fg),
                (compact[i], expected),
                "{wide} cols: glyph {i} of the line"
            );
        }
        // The line closes the card: past it the pane is blank down to its
        // bottom border, so no fix line sits under a wrapped row.
        let bottom = rows
            .iter()
            .enumerate()
            .skip(first)
            .find(|(_, chars)| chars[col0..right].iter().all(|c| *c == '─'))
            .map(|(row, _)| row)
            .expect("the pane's bottom border");
        for (row, chars) in rows
            .iter()
            .enumerate()
            .take(bottom)
            .skip(first + joined.len())
        {
            assert!(
                chars[col0..right].iter().all(|c| *c == ' '),
                "{wide} cols: row {row} under the line is blank: {:?}",
                text(chars)
            );
        }
    }
}
