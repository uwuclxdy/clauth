//! Services-tab render tests. Each row is a dot + label in the selector; the
//! verdict lives in the detail pane. The `herdr` row's verdict logic is
//! unit-tested in `tests/inline/tui_app.rs`; these pin the render per drift
//! state. The delegates detail shows the job list; its rows, overflow marker
//! and empty state are pinned here too.

use crate::herdr::{ConfigStatus, HerdrProbe, RegistryEntry, SidebarState};
use crate::mcp::jobs::{self, JobRecord, JobState, RecordKind, RunningSpec};
use crate::profile::{AppConfig, AppState};
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
            "claude: press r to probe".to_string(),
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
    term.draw(|f| super::draw(f, f.area(), app)).unwrap();
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

    // List on delegates: no `↵ detail`, no `f`.
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
        "a selected delegates row binds no key beyond the shared ones"
    );

    // List on plugin (with a fix): `↵ detail` and the list `f` verb.
    let app = app_with(plugin_check_with_problems());
    assert_eq!(
        hints(&app),
        vec![
            ("↑↓", "row"),
            ("↵", "detail"),
            ("r", "refresh"),
            ("f", "install plugin"),
            ("a", "actions"),
            ("?", "help"),
        ],
        "the list focus advertises the first fixable problem's verb"
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
            ("f", "install plugin"),
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
            ("f", "wire mcp server"),
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

/// The plugin detail folds the four readouts and renders each fix as a dim
/// `f  <verb>` line under its problem — never a bracketed `[f]` cue. The
/// content rows pin by equality (key column 12 cells: widest key `mcp source`
/// + the 2-space gap; blank separators dropped by `detail_rows`).
#[test]
fn the_plugin_detail_folds_the_readouts_and_renders_bare_f_lines() {
    let _home = crate::testutil::HomeSandbox::new();
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

    // The dim `f` line: both `f` and the verb render dim (TEXT_DIM), pinned off
    // the styled buffer rather than the glyph text.
    let row_idx = rows
        .iter()
        .position(|r| r.contains("wire mcp server"))
        .unwrap_or_else(|| panic!("no wire fix line:\n{screen}"));
    let row = &rows[row_idx];
    let byte = row.find("f").expect("f renders");
    let col = row[..byte].chars().count();
    assert_eq!(
        buf.content[row_idx * W as usize + col].fg,
        super::theme::text_dim_color(),
        "an unfocused fix line is whole-dim:\n{screen}"
    );
}

/// Descending into the plugin detail, ↑↓ walks the fixable problems and the
/// focused one takes the caret; `f` fires the focused problem's fix.
#[test]
fn the_plugin_detail_walks_problems_and_f_fixes_the_focused_one() {
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
        dump.contains("f install plugin"),
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

    // `f` from the focused problem opens the install confirm.
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => assert!(
            matches!(
                state.on_confirm,
                crate::tui::app::ConfirmAction::InstallPlugin
            ),
            "f on the focused install problem runs the install"
        ),
        other => panic!("expected an install confirm, got {other:?}"),
    }
    app.modals.clear();

    // Walk to the second problem (wire); the footer and caret follow.
    handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    let dump = dump_full(&app);
    assert!(
        dump.contains("f wire mcp server"),
        "the footer follows the focus to the wire verb:\n{dump}"
    );
    let (rows, _) = render(&app);
    let screen = rows.join("\n");
    assert!(
        rows.iter()
            .any(|r| r.contains("❯") && r.contains("wire mcp server")),
        "the caret follows to the wire problem:\n{screen}"
    );

    // `f` now fires the wire fix.
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => assert!(
            matches!(
                state.on_confirm,
                crate::tui::app::ConfirmAction::WireMcpServers
            ),
            "f on the focused wire problem writes the entry"
        ),
        other => panic!("expected a wire confirm, got {other:?}"),
    }
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

/// From the list, `f` on the plugin row fixes the FIRST fixable problem shown
/// (install), never the second.
#[test]
fn the_list_f_fixes_the_first_fixable_problem() {
    use crate::tui::app::{Modal, handle_key};
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = app_with(plugin_check_with_problems());
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('f')));
    match app.modals.last() {
        Some(Modal::Confirm(state)) => assert!(
            matches!(
                state.on_confirm,
                crate::tui::app::ConfirmAction::InstallPlugin
            ),
            "list f fixes the first fixable problem (install)"
        ),
        other => panic!("expected an install confirm, got {other:?}"),
    }
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
        line_text(&super::delegate_line(cell, 8, 100))
    };
    assert_eq!(
        row("uwuclxdy"),
        "● running   uwuclxdy  elapsed 2m 14s · spawned by cld · idle-kill in 4m 48s  /home/u/repos/app",
        "the run's directory closes the row, never its last words",
    );
    assert_eq!(
        row("kerry"),
        "● blocking  kerry     elapsed 40s · idle-kill in 4m 30s",
        "a record with no origin ends on its facts",
    );
    assert_eq!(
        row("DS8"),
        "● done      DS8       finished 15m ago · spawned by uwuclxdy  /home/u/repos/api",
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

/// More delegates than the detail holds: the last row names the EXACT count that
/// did not fit, computed off the rows that actually rendered.
#[test]
fn the_delegates_detail_marks_its_overflow_with_a_count() {
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
        "the newest delegate is the one kept:\n{screen}"
    );
    assert!(
        !screen.contains("acct21"),
        "the oldest is the one dropped:\n{screen}"
    );
    let marker = rows
        .iter()
        .find(|r| r.contains(" more"))
        .unwrap_or_else(|| panic!("no overflow marker:\n{screen}"));
    let shown = rows.iter().filter(|r| r.contains("● running")).count();
    assert!(
        shown < 22,
        "fixture control: some rows did not fit:\n{screen}"
    );
    assert!(
        marker.contains(&format!("+{} more", 22 - shown)),
        "the marker names the exact hidden count:\n{screen}"
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

/// The pure `delegate_lines` contract: the overflow marker counts exactly, and
/// the run's directory is what gives way (a middle `…`, then the whole of it)
/// when the row runs out of width.
#[test]
fn delegate_lines_pin_the_overflow_count_and_the_directory_exactly() {
    use crate::mcp::jobs::JobPhase;
    use ratatui::text::Line;
    let line_text = |line: &Line| {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    };

    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    for i in 0..5 {
        jobs::write_heartbeat(
            &running_spec(
                &format!("d-t-{i}"),
                &format!("acct{i}"),
                now - 10_000 - i as u64,
                RecordKind::Collectable,
            ),
            now - 1_000 - i as u64,
            "working",
        )
        .unwrap();
    }
    let stored = jobs::list_banded(now);
    let cells = super::delegate_cells(&stored, now);

    // 5 rows into a 3-row viewport: 2 shown, the marker names the other 3.
    let lines = super::delegate_lines(&cells, 3, 100);
    assert_eq!(
        line_text(lines.last().expect("marker line")),
        "+3 more",
        "the marker names the exact hidden count"
    );

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
        line_text(&super::delegate_line(&one, 4, 100)),
        "● running   acct  elapsed 5s  /home/u/repos/rs/clauth",
        "the directory rides whole when it fits",
    );
    // 28 cells of row and a 2-cell gap before the directory: 45 leaves it 15,
    // 42 leaves it exactly `CWD_MIN_W`, 41 one short of it.
    assert_eq!(
        line_text(&super::delegate_line(&one, 4, 45)),
        "● running   acct  elapsed 5s  /home/u…/clauth",
        "a narrow row keeps both ends of the directory",
    );
    assert_eq!(
        line_text(&super::delegate_line(&one, 4, 42)),
        "● running   acct  elapsed 5s  /home/…lauth",
        "the narrowest room a directory still renders in",
    );
    assert_eq!(
        line_text(&super::delegate_line(&one, 4, 41)),
        "● running   acct  elapsed 5s",
        "a row with no room for a readable directory drops it",
    );
}

/// A live row must survive the truncation that a burst of finished ones causes.
///
/// `jobs::list` orders on the retention anchor, which for a `done` record is its
/// FINISH — so every background job that landed a second ago outranks a blocking
/// run that last spoke twenty seconds ago. On anchor order alone the row this
/// detail exists for is the first one evicted, and the detail binds no key, so
/// nothing reaches it afterwards.
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
        rows.iter().any(|r| r.contains("more")),
        "fixture control: more delegates than the detail holds, so something is \
         evicted:\n{screen}"
    );
    assert!(
        screen.contains("kerry") && screen.contains("fresh"),
        "and it is never a live one — BOTH survive, whatever their anchors:\n{screen}"
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

/// The tag-refresh editor renders the edit gutter, the sunken buffer with its
/// unit, and the range sub-line — the Config-tab refresh editor's shape.
#[test]
fn herdr_tag_refresh_editor_renders_the_edit_state() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = herdr_options_app(healthy_config());
    app.services.herdr_options_cursor = 2;
    app.services.herdr_tag_draft = Some(crate::tui::app::InputState::new("5"));
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
