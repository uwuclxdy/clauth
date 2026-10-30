//! `clauth rex install|uninstall`. The init.lua edit is pure text in, text out,
//! which is where a config clauth does not own either survives or gets
//! corrupted; the file half runs against a temp dir standing in for Rex's
//! config directory, so no test touches the environment or a real Rex.

use super::*;
use clap::Parser as _;

use crate::cli::{Cli, Command, RexCommand};

fn paths_in(dir: &Path) -> Paths {
    Paths::resolve(
        Some(dir.join("init.lua").into_os_string()),
        None,
        Path::new("/nonexistent"),
    )
}

#[test]
fn rex_config_wins_over_xdg_and_home() {
    let paths = Paths::resolve(
        Some("/etc/rex/custom.lua".into()),
        Some("/xdg".into()),
        Path::new("/home/u"),
    );
    assert_eq!(paths.init, PathBuf::from("/etc/rex/custom.lua"));
    assert_eq!(paths.plugin, PathBuf::from("/etc/rex/clauth"));
}

#[test]
fn xdg_config_home_then_dot_config() {
    let xdg = Paths::resolve(None, Some("/xdg".into()), Path::new("/home/u"));
    assert_eq!(xdg.init, PathBuf::from("/xdg/rex/init.lua"));
    assert_eq!(xdg.plugin, PathBuf::from("/xdg/rex/clauth"));

    let home = Paths::resolve(Some("".into()), Some("".into()), Path::new("/home/u"));
    assert_eq!(home.init, PathBuf::from("/home/u/.config/rex/init.lua"));
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
    assert_eq!(plugin_dir_state(&paths.plugin), PluginDir::Managed);
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
    assert_eq!(plugin_dir_state(&paths.plugin), PluginDir::Linked);
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
    assert_eq!(plugin_dir_state(&paths.plugin), PluginDir::Absent);

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
    assert_eq!(plugin_dir_state(&paths.plugin), PluginDir::Foreign);
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
