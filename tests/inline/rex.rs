//! Rex: the Services row's reads, and `clauth rex install|uninstall`. The
//! init.lua edit is pure text in, text out, which is where a config clauth
//! does not own either survives or gets corrupted; the file half runs against
//! a temp dir standing in for Rex's config directory, so no test touches the
//! environment or a real Rex.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use clap::Parser as _;
use std::collections::BTreeMap;

use crate::cli::{Cli, Command, RexCommand};

/// A fake process table holding argv only.
struct FakeProcs(BTreeMap<u32, Vec<String>>);

impl FakeProcs {
    fn new(rows: &[(u32, &str)]) -> Self {
        Self(
            rows.iter()
                .map(|&(pid, argv)| (pid, argv.split(' ').map(str::to_owned).collect()))
                .collect(),
        )
    }
}

impl crate::which::ProcTable for FakeProcs {
    fn parent(&self, _pid: u32) -> Option<u32> {
        None
    }
    fn argv(&self, pid: u32) -> Option<Vec<String>> {
        self.0.get(&pid).cloned()
    }
}

const WATCHER: &str = "rex do --all --autostart=false /home/u/.config/rex/clauth/tag-watch.lua";

fn os(s: &str) -> Option<OsString> {
    Some(OsString::from(s))
}

#[test]
fn paths_follow_rex_config_then_xdg_then_home() {
    let home = Path::new("/home/u");
    assert_eq!(
        Paths::resolve(os("/etc/rex/main.lua"), os("/xdg"), home),
        Paths {
            init: PathBuf::from("/etc/rex/main.lua"),
            plugin: PathBuf::from("/etc/rex/clauth"),
        }
    );
    assert_eq!(
        Paths::resolve(None, os("/xdg"), home),
        Paths {
            init: PathBuf::from("/xdg/rex/init.lua"),
            plugin: PathBuf::from("/xdg/rex/clauth"),
        }
    );
    for (rex, xdg) in [(None, None), (os(""), os(""))] {
        assert_eq!(
            Paths::resolve(rex, xdg, home),
            Paths {
                init: PathBuf::from("/home/u/.config/rex/init.lua"),
                plugin: PathBuf::from("/home/u/.config/rex/clauth"),
            }
        );
    }
}

#[test]
fn the_state_dir_follows_xdg_state_home_then_home() {
    let home = Path::new("/home/u");
    assert_eq!(
        state_dir(os("/xdgs"), home),
        PathBuf::from("/xdgs/clauth-rex")
    );
    for xdg in [None, os("")] {
        assert_eq!(
            state_dir(xdg, home),
            PathBuf::from("/home/u/.local/state/clauth-rex")
        );
    }
}

/// A Rex config dir under a temp root, plus a state dir beside it.
struct RexFixture {
    dir: tempfile::TempDir,
}

impl RexFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("rex")).unwrap();
        Self { dir }
    }
    fn paths(&self) -> Paths {
        Paths {
            init: self.dir.path().join("rex").join("init.lua"),
            plugin: self.dir.path().join("rex").join("clauth"),
        }
    }
    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }
    fn managed(&self, marker: &str) {
        let plugin = self.paths().plugin;
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join(".clauth-managed"), marker).unwrap();
    }
    fn pidfile(&self, name: &str, body: &str) {
        std::fs::create_dir_all(self.state()).unwrap();
        std::fs::write(self.state().join(name), body).unwrap();
    }
    fn status(&self, procs: &FakeProcs) -> Option<RexStatus> {
        status_at(&self.paths(), &self.state(), procs)
    }
}

#[test]
fn no_rex_config_dir_means_no_row() {
    let fx = RexFixture::new();
    std::fs::remove_dir(fx.dir.path().join("rex")).unwrap();
    assert_eq!(fx.status(&FakeProcs::new(&[])), None);
}

#[test]
fn a_config_dir_without_the_plugin_reads_absent() {
    let fx = RexFixture::new();
    assert_eq!(
        fx.status(&FakeProcs::new(&[])),
        Some(RexStatus {
            plugin: PluginDir::Absent,
            watcher_running: false,
        })
    );
}

#[test]
fn a_managed_dir_reads_its_marker_version() {
    let fx = RexFixture::new();
    fx.managed("written by clauth 0.17.0\n");
    assert_eq!(
        fx.status(&FakeProcs::new(&[])).map(|s| s.plugin),
        Some(PluginDir::Managed {
            version: "0.17.0".to_owned(),
        })
    );
}

/// A marker whose first line names no version proves nothing about which
/// clauth wrote the dir.
#[test]
fn a_marker_whose_first_line_is_not_the_version_reads_foreign() {
    for marker in [
        "",
        "garbage\nwritten by clauth 0.17.0\n",
        "written by clauth \n",
    ] {
        let fx = RexFixture::new();
        fx.managed(marker);
        assert_eq!(
            fx.status(&FakeProcs::new(&[])).map(|s| s.plugin),
            Some(PluginDir::Foreign),
            "marker {marker:?}"
        );
    }
}

/// A marker this user cannot read names no version either.
#[cfg(unix)]
#[test]
fn a_marker_this_user_cannot_read_reads_foreign() {
    use std::os::unix::fs::PermissionsExt;
    let fx = RexFixture::new();
    fx.managed("written by clauth 0.17.0\n");
    let marker = fx.paths().plugin.join(".clauth-managed");
    std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&marker).is_ok() {
        // Root reads through any mode, so the fault cannot be posed here.
        return;
    }
    assert_eq!(
        fx.status(&FakeProcs::new(&[])).map(|s| s.plugin),
        Some(PluginDir::Foreign)
    );
}

#[test]
fn a_dir_without_the_marker_reads_foreign() {
    let fx = RexFixture::new();
    std::fs::create_dir_all(fx.paths().plugin).unwrap();
    assert_eq!(
        fx.status(&FakeProcs::new(&[])).map(|s| s.plugin),
        Some(PluginDir::Foreign)
    );
}

#[test]
fn a_marker_that_is_no_regular_file_reads_foreign() {
    let fx = RexFixture::new();
    std::fs::create_dir_all(fx.paths().plugin.join(".clauth-managed")).unwrap();
    assert_eq!(
        fx.status(&FakeProcs::new(&[])).map(|s| s.plugin),
        Some(PluginDir::Foreign)
    );
}

#[test]
fn a_regular_file_at_the_plugin_path_reads_foreign() {
    let fx = RexFixture::new();
    std::fs::write(fx.paths().plugin, b"not a dir").unwrap();
    assert_eq!(
        fx.status(&FakeProcs::new(&[])).map(|s| s.plugin),
        Some(PluginDir::Foreign)
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_plugin_dir_reads_linked_with_its_target() {
    let fx = RexFixture::new();
    let checkout = fx.dir.path().join("checkout").join("rex-plugin");
    std::fs::create_dir_all(&checkout).unwrap();
    // A marker inside the target must not turn a link into a managed dir.
    std::fs::write(
        checkout.join(".clauth-managed"),
        "written by clauth 0.18.1\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&checkout, fx.paths().plugin).unwrap();
    assert_eq!(
        fx.status(&FakeProcs::new(&[])).map(|s| s.plugin),
        Some(PluginDir::Linked {
            target: Some(checkout),
            missing: false,
        })
    );
}

/// A link whose target cannot be reached loads nothing: the row must say so.
#[cfg(unix)]
#[test]
fn a_dangling_plugin_link_reads_linked_with_a_missing_target() {
    let fx = RexFixture::new();
    let gone = fx.dir.path().join("gone").join("rex-plugin");
    std::os::unix::fs::symlink(&gone, fx.paths().plugin).unwrap();
    assert_eq!(
        fx.status(&FakeProcs::new(&[])).map(|s| s.plugin),
        Some(PluginDir::Linked {
            target: Some(gone),
            missing: true,
        })
    );
}

/// A link that resolves to a regular file loads no plugin (Rex loads a
/// folder): the read says so rather than reading as a working link.
#[cfg(unix)]
#[test]
fn a_plugin_link_onto_a_regular_file_reads_not_a_dir() {
    let fx = RexFixture::new();
    let file = fx.dir.path().join("checkout").join("init.lua");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "-- not a plugin dir\n").unwrap();
    std::os::unix::fs::symlink(&file, fx.paths().plugin).unwrap();
    assert_eq!(
        fx.status(&FakeProcs::new(&[])).map(|s| s.plugin),
        Some(PluginDir::LinkedNotDir { target: Some(file) })
    );
}

#[test]
fn a_pidfile_naming_a_live_watcher_reads_running() {
    let fx = RexFixture::new();
    fx.pidfile("watch-12345.pid", "4242\n");
    assert_eq!(
        fx.status(&FakeProcs::new(&[(4242, WATCHER)]))
            .map(|s| s.watcher_running),
        Some(true)
    );
}

#[test]
fn any_one_live_watcher_among_several_pidfiles_reads_running() {
    let fx = RexFixture::new();
    fx.pidfile("watch-1.pid", "100\n");
    fx.pidfile("watch-2.pid", "200\n");
    fx.pidfile("watch-3.pid", "300\n");
    assert_eq!(
        fx.status(&FakeProcs::new(&[(100, "bash"), (300, WATCHER)]))
            .map(|s| s.watcher_running),
        Some(true)
    );
}

#[test]
fn a_watcher_is_not_running_unless_a_pidfile_names_a_live_tag_watch() {
    // No state dir at all.
    let fx = RexFixture::new();
    assert_eq!(
        fx.status(&FakeProcs::new(&[(4242, WATCHER)]))
            .map(|s| s.watcher_running),
        Some(false)
    );

    let procs = FakeProcs::new(&[(4242, WATCHER), (777, "vim notes.txt")]);
    for (name, body) in [
        // A recycled pid running something else.
        ("watch-1.pid", "777\n"),
        // A pid that is gone.
        ("watch-1.pid", "999\n"),
        // Not a pid.
        ("watch-1.pid", "abc\n"),
        ("watch-1.pid", ""),
        // A live watcher's pid in a file that is no watcher pidfile.
        ("watch.log", "4242\n"),
        ("watch-1.lock", "4242\n"),
        ("other.pid", "4242\n"),
    ] {
        let fx = RexFixture::new();
        fx.pidfile(name, body);
        assert_eq!(
            fx.status(&procs).map(|s| s.watcher_running),
            Some(false),
            "{name} holding {body:?}"
        );
    }
}

fn paths_in(dir: &Path) -> Paths {
    Paths::resolve(
        Some(dir.join("init.lua").into_os_string()),
        None,
        Path::new("/nonexistent"),
    )
}

#[test]
fn block_appends_to_an_existing_config_and_keeps_it() {
    let existing = "rex.bind(\"cmd+e\", \"pane.zoom\")";
    let text = with_block(existing).expect("appended");
    assert!(text.starts_with("rex.bind(\"cmd+e\", \"pane.zoom\")\n"));
    assert!(text.ends_with(&block()));
}

#[test]
fn block_is_not_appended_twice() {
    let once = with_block("").expect("appended");
    assert_eq!(with_block(&once), None);
}

#[test]
fn a_hand_written_require_is_left_alone() {
    for existing in [
        "require(\"clauth\").setup{ tag = false }\n",
        "local c = require('clauth')\nc.setup{}\n",
    ] {
        assert!(loads_plugin(existing), "{existing}");
        assert_eq!(with_block(existing), None, "{existing}");
    }
    // A commented-out require does not load anything.
    assert!(!loads_plugin("-- require(\"clauth\").setup{}\n"));
}

#[test]
fn removing_the_block_restores_the_file() {
    let existing = "rex.bind(\"cmd+e\", \"pane.zoom\")\n";
    let wired = with_block(existing).expect("appended");
    assert_eq!(without_block(&wired), (existing.to_string(), true));

    let around = format!("-- before\n{}-- after\n", block());
    assert_eq!(
        without_block(&around),
        ("-- before\n-- after\n".to_string(), true)
    );
}

#[test]
fn an_unterminated_block_is_left_whole() {
    let cut = format!("{BLOCK_BEGIN}\nrequire(\"clauth\").setup{{}}\n");
    assert_eq!(without_block(&cut), (cut.clone(), false));
}

#[test]
fn install_writes_the_plugin_and_wires_init_lua() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    std::fs::write(&paths.init, "rex.bind(\"cmd+e\", \"pane.zoom\")\n").expect("seed init.lua");

    assert!(install_at(&paths, true).expect("install"));
    for (name, contents, _) in FILES {
        let written = std::fs::read_to_string(paths.plugin.join(name)).expect("plugin file");
        assert_eq!(&written, contents, "{name}");
    }
    assert_eq!(
        plugin_at(&paths.plugin),
        PluginDir::Managed {
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        "the Services row reads the version install wrote"
    );
    let init = std::fs::read_to_string(&paths.init).expect("init.lua");
    assert!(init.starts_with("rex.bind(\"cmd+e\", \"pane.zoom\")\n"));
    assert_eq!(init.matches(BLOCK_BEGIN).count(), 1);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(paths.plugin.join("start-watch.sh"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "scripts are executable");
    }

    // A second install refreshes the files and leaves init.lua as it was.
    assert!(install_at(&paths, true).expect("reinstall"));
    assert_eq!(
        std::fs::read_to_string(&paths.init).expect("init.lua"),
        init
    );
}

#[test]
fn install_creates_init_lua_when_there_is_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(&tmp.path().join("rex"));
    assert!(install_at(&paths, true).expect("install"));
    assert_eq!(
        std::fs::read_to_string(&paths.init).expect("init.lua"),
        block()
    );
}

#[test]
fn install_refuses_a_directory_clauth_did_not_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    std::fs::create_dir_all(&paths.plugin).expect("foreign dir");
    std::fs::write(paths.plugin.join("init.lua"), "-- mine\n").expect("foreign file");

    let err = install_at(&paths, true).expect_err("refused");
    assert!(err.to_string().contains("clauth did not write it"), "{err}");
    assert_eq!(
        std::fs::read_to_string(paths.plugin.join("init.lua")).expect("kept"),
        "-- mine\n"
    );
    assert!(!paths.init.exists(), "init.lua untouched");
}

#[cfg(unix)]
#[test]
fn install_refuses_a_symlinked_checkout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let checkout = tmp.path().join("checkout");
    std::fs::create_dir_all(&checkout).expect("checkout");
    std::os::unix::fs::symlink(&checkout, &paths.plugin).expect("link");

    let err = install_at(&paths, true).expect_err("refused");
    assert!(err.to_string().contains("symlink"), "{err}");
    assert!(matches!(
        plugin_at(&paths.plugin),
        PluginDir::Linked { missing: false, .. }
    ));
}

#[test]
fn uninstall_reverses_install() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let original = "rex.bind(\"cmd+e\", \"pane.zoom\")\n";
    std::fs::write(&paths.init, original).expect("seed init.lua");

    assert!(install_at(&paths, true).expect("install"));
    assert!(uninstall_at(&paths, true).expect("uninstall"));
    assert_eq!(
        std::fs::read_to_string(&paths.init).expect("init.lua"),
        original
    );
    assert_eq!(plugin_at(&paths.plugin), PluginDir::Absent);

    // Nothing left to remove is not an error.
    assert!(!uninstall_at(&paths, true).expect("second uninstall"));
}

#[test]
fn uninstall_leaves_a_foreign_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    std::fs::write(&paths.init, block()).expect("seed init.lua");
    std::fs::create_dir_all(&paths.plugin).expect("foreign dir");

    assert!(uninstall_at(&paths, true).expect("uninstall"));
    assert_eq!(std::fs::read_to_string(&paths.init).expect("init.lua"), "");
    assert_eq!(plugin_at(&paths.plugin), PluginDir::Foreign);
}

#[test]
fn the_cli_parses_both_subcommands() {
    let cli = Cli::try_parse_from(["clauth", "rex", "install", "--yes"]).expect("parse");
    assert!(matches!(
        cli.command,
        Some(Command::Rex {
            cmd: RexCommand::Install { yes: true }
        })
    ));
    let cli = Cli::try_parse_from(["clauth", "rex", "uninstall"]).expect("parse");
    assert!(matches!(
        cli.command,
        Some(Command::Rex {
            cmd: RexCommand::Uninstall { yes: false }
        })
    ));
}

/// Runs the real `rex-plugin/resolve-profile.sh` for pid 100, faked in a
/// `/proc` of its own as `argv` under pid 1, against a sandbox `$HOME` whose
/// live-session registry holds `rows`. `adopted_codex = Some(name)` lays
/// `~/.codex/auth.json` as the link `clauth login <name> --codex` leaves, and
/// `which` is what a `clauth which` shim on the script's PATH answers.
#[cfg(unix)]
fn resolve(argv: &str, rows: &[&str], adopted_codex: Option<&str>, which: &str) -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    let proc_dir = home.join("proc/100");
    std::fs::create_dir_all(&proc_dir).expect("proc dir");
    std::fs::write(proc_dir.join("stat"), "100 (a) S 1 100 100 0 -1\n").expect("stat");
    let mut cmdline = argv.replace(' ', "\0");
    cmdline.push('\0');
    std::fs::write(proc_dir.join("cmdline"), cmdline).expect("cmdline");
    let sessions = home.join(".clauth/live_sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    for (i, row) in rows.iter().enumerate() {
        std::fs::write(sessions.join(format!("s{i}.json")), row).expect("row");
    }
    if let Some(name) = adopted_codex {
        let store = home.join(".clauth/profiles").join(name);
        std::fs::create_dir_all(&store).expect("profile store");
        std::fs::write(store.join("auth.json"), "{}").expect("chain");
        std::fs::create_dir_all(home.join(".codex")).expect("codex home");
        std::os::unix::fs::symlink(store.join("auth.json"), home.join(".codex/auth.json"))
            .expect("adopted link");
    }
    // The script puts $HOME/.local/bin first on its PATH, so the shim answers
    // ahead of any real clauth.
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).expect("bin dir");
    let shim = bin.join("clauth");
    std::fs::write(
        &shim,
        format!("#!/bin/sh\n[ \"$1\" = which ] && echo {which}\n"),
    )
    .expect("shim");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let out = std::process::Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/rex-plugin/resolve-profile.sh"
        ))
        .arg("100")
        .env("HOME", home)
        .env("CLAUTH_PROC_ROOT", home.join("proc"))
        .env_remove("CODEX_HOME")
        .output()
        .expect("resolve-profile.sh runs");
    assert!(
        out.status.success(),
        "exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8")
}

#[cfg(unix)]
fn row(harness: Option<&str>) -> String {
    let harness = harness.map_or(String::new(), |h| format!(r#""harness":"{h}","#));
    format!(
        r#"{{"session_id":"s0","start_profile":"work",{harness}"pid":100,"follows_chain":false,"current_member":null}}"#
    )
}

#[cfg(unix)]
#[test]
fn a_codex_session_clauth_started_names_the_codex_agent() {
    // `clauth start <codex profile>`: the pane runs the clauth supervisor, so
    // only the row's harness says the agent is codex, not Claude Code.
    let out = resolve("clauth start work", &[&row(Some("codex"))], None, "main");
    assert_eq!(out, "work\tclauth\tcodex\n");
}

#[cfg(unix)]
#[test]
fn a_claude_session_clauth_started_names_the_claude_agent() {
    let out = resolve("clauth start work", &[&row(Some("claude"))], None, "main");
    assert_eq!(out, "work\tclauth\tclaude\n");
}

#[cfg(unix)]
#[test]
fn a_row_from_before_the_harness_field_is_claude() {
    let out = resolve("clauth start work", &[&row(None)], None, "main");
    assert_eq!(out, "work\tclauth\tclaude\n");
}

#[cfg(unix)]
#[test]
fn a_bare_codex_on_an_adopted_login_names_its_profile() {
    let out = resolve("codex", &[], Some("work"), "main");
    assert_eq!(out, "work\tcodex\tcodex\n");
}

#[cfg(unix)]
#[test]
fn a_bare_codex_on_its_own_login_spends_no_account() {
    let out = resolve("codex", &[], None, "main");
    assert_eq!(out, "");
}

#[cfg(unix)]
#[test]
fn a_bare_claude_spends_the_global_login() {
    let out = resolve("claude", &[], None, "main");
    assert_eq!(out, "main\tglobal\tclaude\n");
}
