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

/// The watcher state dir beside a [`paths_in`] config.
fn state_in(dir: &Path) -> PathBuf {
    dir.join("state")
}

/// `text` read as `init.lua`, for a case that expects it to read.
fn read(text: &str) -> InitLua<'_> {
    InitLua::read(text).expect("reads")
}

fn wire(text: &str) -> Option<String> {
    read(text).with_block()
}

fn unwire(text: &str) -> String {
    read(text).without_blocks()
}

fn loads(text: &str) -> bool {
    read(text).loads_plugin()
}

/// Why `text` does not read, and the 1-based line it names.
fn refused(text: &str) -> (Why, usize) {
    let refusal = InitLua::read(text).expect_err("refused");
    (refusal.why, refusal.line)
}

#[test]
fn block_appends_to_an_existing_config_and_keeps_it() {
    let existing = "rex.bind(\"cmd+e\", \"pane.zoom\")\n";
    let text = wire(existing).expect("appended");
    assert_eq!(text, format!("{existing}{}", block()));
}

/// A file with no final newline gets the block on lines of its own, and
/// gets its missing newline back when the block comes out.
#[test]
fn a_config_without_a_final_newline_round_trips() {
    let existing = "rex.bind(\"cmd+e\", \"pane.zoom\")";
    let text = wire(existing).expect("appended");
    assert!(text.starts_with(&format!("{existing}\n{BLOCK_BEGIN}\n")));
    assert_eq!(read(&text).blocks, vec![(1, 3)]);
    assert_eq!(unwire(&text), existing);

    // A line written after the block keeps everything before it as it was.
    let edited = format!("{text}\nrex.bind(\"cmd+k\", \"pane.close\")\n");
    assert_eq!(
        unwire(&edited),
        format!("{existing}\nrex.bind(\"cmd+k\", \"pane.close\")\n")
    );
}

/// Lua ends a line at a lone `\r` as at a `\n`, so a file with `\r`-only
/// line ends is lines, not one long comment, and round-trips byte for byte.
#[test]
fn a_config_with_cr_only_line_ends_round_trips() {
    let existing = "-- mine\rrex.bind(\"cmd+e\", \"pane.zoom\")\r";
    let text = wire(existing).expect("appended");
    assert_eq!(text, format!("{existing}{}", block()));
    assert_eq!(read(&text).blocks, vec![(2, 4)]);
    assert_eq!(unwire(&text), existing);
}

#[test]
fn block_is_not_appended_twice() {
    let once = wire("").expect("appended");
    assert_eq!(wire(&once), None);
}

/// Install and uninstall read the markers alike: a begin line an editor left
/// a trailing space or a CRLF on is still the block, so a reinstall appends
/// no second one, and uninstall takes it out.
#[test]
fn a_marker_line_with_trailing_whitespace_is_still_the_block() {
    for end in [" ", "\t", "\r"] {
        let text = format!(
            "-- mine\n{BLOCK_BEGIN}{end}\nrequire(\"clauth\").setup{{}}\n{BLOCK_END}{end}\n"
        );
        assert_eq!(wire(&text), None, "{end:?}");
        assert_eq!(unwire(&text), "-- mine\n", "{end:?}");
    }
}

#[test]
fn a_hand_written_require_is_left_alone() {
    for existing in [
        "require(\"clauth\").setup{ tag = false }\n",
        "local c = require('clauth')\nc.setup{}\n",
        "require\"clauth\".setup{}\n",
        "require 'clauth'.setup{}\n",
        "local c = require ( \"clauth\" )\n",
        "require [[clauth]].setup{}\n",
        "require [==[clauth]==].setup{}\n",
        // Split over lines, a comment between.
        "require(\n  \"clauth\"\n).setup{}\n",
        "require --[[ the plugin ]] \"clauth\"\n",
        // A `--` inside a string starts no comment.
        "local s = \"--\"; require(\"clauth\").setup{}\n",
        "local s = 'it\\'s --[[ no comment'; require(\"clauth\").setup{}\n",
        // Code after a block comment's close is code.
        "--[[ old ]] require(\"clauth\").setup{}\n",
        // Lua ends a line comment at a lone `\r`, so this loads the module.
        "-- note\rrequire(\"clauth\").setup{}\n",
    ] {
        assert!(loads(existing), "{existing:?}");
        assert_eq!(wire(existing), None, "{existing:?}");
    }
}

/// A require that sits in a comment or a string, or merely names a longer
/// module, loads nothing, so install still appends the block.
#[test]
fn a_require_that_loads_nothing_is_not_taken_for_one() {
    for existing in [
        "-- require(\"clauth\").setup{}\n",
        "--[[\nrequire(\"clauth\").setup{}\n]]\n",
        "--[==[\nrequire(\"clauth\").setup{}\n]] still the comment\n]==]\n",
        "--[[ require('clauth') ]]\n",
        "local s = [[\nrequire(\"clauth\").setup{}\n]]\n",
        "local s = \"require('clauth')\"\n",
        "require(\"clauth.extra\")\n",
        "my_require(\"clauth\")\n",
    ] {
        assert!(!loads(existing), "{existing:?}");
        assert!(wire(existing).is_some(), "{existing:?}");
    }
}

#[test]
fn the_loading_line_is_named_by_its_place_in_the_file() {
    let existing = "--[[\nnotes\n]]\nlocal c = require(\"clauth\")  \nc.setup{}\n";
    assert_eq!(
        read(existing).still_loaded(),
        Some(Refusal {
            line: 4,
            text: "local c = require(\"clauth\")".to_owned(),
            why: Why::StillLoaded,
        })
    );
    // clauth's own block is no hand-written require.
    assert_eq!(read(&format!("-- mine\n{}", block())).still_loaded(), None);
}

#[test]
fn removing_the_block_restores_the_file() {
    let existing = "rex.bind(\"cmd+e\", \"pane.zoom\")\n";
    let wired = wire(existing).expect("appended");
    assert_eq!(unwire(&wired), existing);

    let around = format!("-- before\n{}-- after\n", block());
    assert_eq!(unwire(&around), "-- before\n-- after\n");
}

/// A marker counts only where Lua reads a comment that starts its line: a
/// copy of the block inside a user's long comment or long string, or one a
/// lone `\r` puts inside a long comment, is the user's text, and stays.
#[test]
fn a_block_inside_a_comment_or_a_string_is_not_the_block() {
    let block = block();
    for user in [
        format!("--[[\n{block}]]\n"),
        format!("--[==[ old setup:\n{block}]==]\n"),
        format!("local s = [=[\n{block}]=]\n"),
        format!("-- note\r--[[\n{block}]]\n"),
    ] {
        let text = format!("{user}local keep = 1\n{block}");
        assert_eq!(unwire(&text), format!("{user}local keep = 1\n"), "{user:?}");
        assert_eq!(wire(&user), Some(format!("{user}{block}")), "{user:?}");
    }
}

/// A marker after code on its line, or indented, is a comment like any other.
#[test]
fn a_marker_that_does_not_start_its_line_is_not_one() {
    for line in [
        format!("local x = 1 {BLOCK_BEGIN}"),
        format!("  {BLOCK_END}"),
    ] {
        let text = format!("{line}\n");
        assert_eq!(read(&text).blocks, Vec::new(), "{line:?}");
        assert_eq!(unwire(&text), text, "{line:?}");
    }
}

/// Each begin line reaches its end line before the next begin. Any other
/// shape is refused, naming the line, before install or uninstall changes
/// a file.
#[test]
fn markers_that_do_not_pair_up_are_refused() {
    let block = block();
    // The begin line cut off before a whole block: pairing it with the
    // block's end line would take the user's lines between them too.
    assert_eq!(
        refused(&format!("-- mine\n{BLOCK_BEGIN}\nlocal keep = 1\n{block}")),
        (Why::BeginAgain { next: 4 }, 2)
    );
    assert_eq!(
        refused(&format!(
            "-- mine\n{BLOCK_BEGIN}\nrequire(\"clauth\").setup{{}}\n"
        )),
        (Why::Unterminated, 2)
    );
    assert_eq!(
        refused(&format!("-- mine\n{BLOCK_END}\n{block}")),
        (Why::StrayEnd, 2)
    );
    assert_eq!(
        refused(&format!("{block}{BLOCK_END}\n")),
        (Why::StrayEnd, 4)
    );
}

/// Two whole blocks both go.
#[test]
fn every_whole_block_comes_out() {
    let block = block();
    assert_eq!(
        unwire(&format!("-- a\n{block}-- b\n{block}-- c\n")),
        "-- a\n-- b\n-- c\n"
    );
}

/// A file Rex's lexer rejects is refused too: nothing in it says where its
/// code stops and its comments start.
#[test]
fn a_file_that_does_not_lex_is_refused() {
    for (text, why, line) in [
        ("-- mine\nlocal s = \"no end\n", Why::UnfinishedString, 2),
        ("local s = 'no end\rlocal t = 1\n", Why::UnfinishedString, 1),
        ("local s = \"no end", Why::UnfinishedString, 1),
        ("-- mine\n--[[ no end\n", Why::UnfinishedLong, 2),
        ("local s = [==[ no end ]=]\n", Why::UnfinishedLong, 1),
        ("--[=x\n", Why::BadLongBracket, 1),
        ("local t = {}\nt[=1]\n", Why::BadLongBracket, 2),
    ] {
        assert_eq!(refused(text), (why, line), "{text:?}");
    }
}

/// Lua 5.1's escapes, as Rex's lexer reads them: an escaped quote or line
/// end stays inside the string, and a decimal escape is one byte.
#[test]
fn string_escapes_decode_as_rex_reads_them() {
    let value = |text: &str| match &read(text).tokens[0].lexeme {
        Lexeme::Str(value) => value.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(value("\"a\\\"b\""), b"a\"b");
    assert_eq!(value("'a\\\r\nb'"), b"a\nb");
    assert_eq!(value("\"\\99lauth\""), b"clauth");
    assert_eq!(value("\"\\x63\""), b"x63");
    assert_eq!(value("[[\r\nfirst\r\nsecond]]"), b"first\nsecond");
    assert_eq!(
        read("#!/usr/bin/env lua 'it\nlocal x = 1\n").tokens.len(),
        4
    );
}

#[test]
fn install_writes_the_plugin_and_wires_init_lua() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    std::fs::write(&paths.init, "rex.bind(\"cmd+e\", \"pane.zoom\")\n").expect("seed init.lua");

    assert!(install_at(&paths, &state, true).expect("install"));
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
    assert!(install_at(&paths, &state, true).expect("reinstall"));
    assert_eq!(
        std::fs::read_to_string(&paths.init).expect("init.lua"),
        init
    );
}

#[test]
fn install_creates_init_lua_when_there_is_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(&tmp.path().join("rex"));
    let state = state_in(&tmp.path().join("rex"));
    assert!(install_at(&paths, &state, true).expect("install"));
    assert_eq!(
        std::fs::read_to_string(&paths.init).expect("init.lua"),
        block()
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_init_lua_keeps_its_link_and_mode() {
    use std::os::unix::fs::PermissionsExt;

    // A dotfiles checkout: init.lua is a link onto the tracked file.
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(&tmp.path().join("rex"));
    let state = state_in(&tmp.path().join("rex"));
    let tracked = tmp.path().join("dotfiles/init.lua");
    std::fs::create_dir_all(tracked.parent().expect("parent")).expect("dotfiles dir");
    let original = "rex.bind(\"cmd+e\", \"pane.zoom\")\n";
    std::fs::write(&tracked, original).expect("seed tracked file");
    std::fs::set_permissions(&tracked, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    std::fs::create_dir_all(paths.init.parent().expect("parent")).expect("rex dir");
    std::os::unix::fs::symlink(&tracked, &paths.init).expect("link");

    let still_linked = || {
        std::fs::symlink_metadata(&paths.init)
            .expect("meta")
            .file_type()
            .is_symlink()
    };
    let mode = || {
        std::fs::metadata(&tracked)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777
    };

    assert!(install_at(&paths, &state, true).expect("install"));
    assert!(still_linked(), "install kept the link");
    assert_eq!(mode(), 0o600, "install kept the mode");
    let wired = std::fs::read_to_string(&tracked).expect("tracked file");
    assert_eq!(wired, wire(original).expect("appended"));

    assert!(uninstall_at(&paths, &state, true).expect("uninstall"));
    assert!(still_linked(), "uninstall kept the link");
    assert_eq!(mode(), 0o600, "uninstall kept the mode");
    assert_eq!(
        std::fs::read_to_string(&tracked).expect("tracked file"),
        original
    );
}

#[test]
fn install_refuses_a_directory_clauth_did_not_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    std::fs::create_dir_all(&paths.plugin).expect("foreign dir");
    std::fs::write(paths.plugin.join("init.lua"), "-- mine\n").expect("foreign file");

    let err = install_at(&paths, &state, true).expect_err("refused");
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
    let state = state_in(tmp.path());
    let checkout = tmp.path().join("checkout");
    std::fs::create_dir_all(&checkout).expect("checkout");
    std::os::unix::fs::symlink(&checkout, &paths.plugin).expect("link");

    let err = install_at(&paths, &state, true).expect_err("refused");
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
    let state = state_in(tmp.path());
    let original = "rex.bind(\"cmd+e\", \"pane.zoom\")\n";
    std::fs::write(&paths.init, original).expect("seed init.lua");

    assert!(install_at(&paths, &state, true).expect("install"));
    assert!(uninstall_at(&paths, &state, true).expect("uninstall"));
    assert_eq!(
        std::fs::read_to_string(&paths.init).expect("init.lua"),
        original
    );
    assert_eq!(plugin_at(&paths.plugin), PluginDir::Absent);

    // Nothing left to remove is not an error.
    assert!(!uninstall_at(&paths, &state, true).expect("second uninstall"));
}

#[test]
fn uninstall_leaves_a_foreign_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    std::fs::write(&paths.init, block()).expect("seed init.lua");
    std::fs::create_dir_all(&paths.plugin).expect("foreign dir");

    assert!(uninstall_at(&paths, &state, true).expect("uninstall"));
    assert_eq!(std::fs::read_to_string(&paths.init).expect("init.lua"), "");
    assert_eq!(plugin_at(&paths.plugin), PluginDir::Foreign);
}

/// The options recipe: a hand-written `require("clauth").setup{ ... }` in
/// place of the block. Removing `clauth/` under it would leave Rex's next
/// config load requiring a module that is gone, so uninstall refuses before
/// it changes anything, and names the line.
#[test]
fn uninstall_refuses_while_init_lua_still_loads_the_plugin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    assert!(install_at(&paths, &state, true).expect("install"));
    let hand_written =
        "rex.bind(\"cmd+e\", \"pane.zoom\")\nrequire(\"clauth\").setup{ tag = false }\n";
    for existing in [
        hand_written.to_string(),
        // Beside the block too: taking the block out leaves this one.
        format!("{hand_written}{}", block()),
    ] {
        std::fs::write(&paths.init, &existing).expect("seed init.lua");
        let err = uninstall_at(&paths, &state, true).expect_err("refused");
        let message = err.to_string();
        assert!(
            message.starts_with(&format!(
                "{}:2 loads the plugin outside clauth's block:\n",
                paths.init.display()
            )),
            "{message}"
        );
        assert!(
            message.contains("require(\"clauth\").setup{ tag = false }"),
            "{message}"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.init).expect("init.lua"),
            existing,
            "init.lua untouched"
        );
        assert!(
            matches!(plugin_at(&paths.plugin), PluginDir::Managed { .. }),
            "the plugin stays"
        );
    }
}

/// A block whose end line was cut has no end to stop at, so uninstall
/// refuses it rather than guess, before it changes anything.
#[test]
fn uninstall_refuses_an_unterminated_block() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    assert!(install_at(&paths, &state, true).expect("install"));
    let cut = format!("-- mine\n{BLOCK_BEGIN}\nrequire(\"clauth\").setup{{}}\n");
    std::fs::write(&paths.init, &cut).expect("seed init.lua");

    let err = uninstall_at(&paths, &state, true).expect_err("refused");
    assert!(
        err.to_string().starts_with(&format!(
            "{}:2 begins clauth's block, but no end line closes it:\n",
            paths.init.display()
        )),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&paths.init).expect("init.lua"), cut);
    assert!(matches!(
        plugin_at(&paths.plugin),
        PluginDir::Managed { .. }
    ));
}

/// The spellings of a load a line-by-line read missed: each refuses, naming
/// the line the load starts on, and leaves init.lua and `clauth/` as they
/// were.
#[test]
fn uninstall_refuses_a_load_whatever_its_line_ends() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    assert!(install_at(&paths, &state, true).expect("install"));
    for (existing, line) in [
        ("-- mine\nrequire(\n  \"clauth\"\n).setup{}\n", 2),
        ("-- note\rrequire(\"clauth\").setup{}\n", 2),
        ("-- mine\rrequire(\"clauth\").setup{ tag = false }\r", 2),
    ] {
        std::fs::write(&paths.init, existing).expect("seed init.lua");
        let err = uninstall_at(&paths, &state, true).expect_err("refused");
        assert!(
            err.to_string().starts_with(&format!(
                "{}:{line} loads the plugin outside clauth's block:\n",
                paths.init.display()
            )),
            "{existing:?}: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.init).expect("init.lua"),
            existing,
            "init.lua untouched"
        );
        assert!(matches!(
            plugin_at(&paths.plugin),
            PluginDir::Managed { .. }
        ));
    }
}

/// A copy of the block inside the user's own long comment is theirs:
/// uninstall takes out only the real block below it.
#[test]
fn uninstall_keeps_a_commented_out_copy_of_the_block() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    let mine = format!("--[[ before the plugin:\n{}]]\nlocal keep = 1\n", block());
    std::fs::write(&paths.init, &mine).expect("seed init.lua");
    assert!(install_at(&paths, &state, true).expect("install"));
    assert!(uninstall_at(&paths, &state, true).expect("uninstall"));
    assert_eq!(
        std::fs::read_to_string(&paths.init).expect("init.lua"),
        mine
    );
    assert_eq!(plugin_at(&paths.plugin), PluginDir::Absent);
}

/// A plugin dir clauth did not write stays whatever uninstall does, so a
/// hand-written require of it keeps loading and is no reason to refuse.
#[cfg(unix)]
#[test]
fn a_hand_written_require_of_a_linked_checkout_does_not_block_uninstall() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    let checkout = tmp.path().join("checkout");
    std::fs::create_dir_all(&checkout).expect("checkout");
    std::os::unix::fs::symlink(&checkout, &paths.plugin).expect("link");
    let hand_written = "require(\"clauth\").setup{ tag = false }\n";
    std::fs::write(&paths.init, format!("{hand_written}{}", block())).expect("seed init.lua");

    assert!(uninstall_at(&paths, &state, true).expect("uninstall"));
    assert_eq!(
        std::fs::read_to_string(&paths.init).expect("init.lua"),
        hand_written
    );
}

/// A stand-in watcher: a child whose argv names `tag-watch.lua`, the way
/// start-watch.sh's `rex do ... tag-watch.lua` does. The loop keeps sh from
/// exec-ing into `sleep`, which would drop the name from the argv.
#[cfg(unix)]
fn fake_watcher() -> std::process::Child {
    // Null stdio: the TERM stops `sh`, but its `sleep` lives on for up to a
    // second, and holding the test's stdout it would read as a leak.
    std::process::Command::new("sh")
        .args(["-c", "while :; do sleep 1; done", "tag-watch.lua"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn fake watcher")
}

#[cfg(unix)]
fn record(state: &Path, key: &str, pid: u32) -> PathBuf {
    std::fs::create_dir_all(state).expect("state dir");
    let pidfile = state.join(format!("watch-{key}.pid"));
    std::fs::write(&pidfile, format!("{pid}\n")).expect("pidfile");
    pidfile
}

/// Waits for `child` to exit, killing it if the test's own stop did not.
#[cfg(unix)]
fn exited(child: &mut std::process::Child) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if child.try_wait().expect("try_wait").is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

#[cfg(unix)]
#[test]
fn a_pidfile_naming_another_program_is_not_a_watcher() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    // A reused pid: the pidfile names a live process that is not a watcher.
    let mut other = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    record(&state, "1", other.id());
    std::fs::write(state.join("watch-2.pid"), "not a pid\n").expect("junk pidfile");
    std::fs::write(state.join("watch.log"), "").expect("log");

    assert_eq!(
        running_watchers(&state, &crate::which::SystemProcs),
        Vec::new()
    );
    assert!(
        other.try_wait().expect("try_wait").is_none(),
        "left running"
    );
    let _ = other.kill();
    let _ = other.wait();
}

#[cfg(unix)]
#[test]
fn install_stops_the_running_watcher() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    assert!(install_at(&paths, &state, true).expect("install"));

    let mut watcher = fake_watcher();
    let pidfile = record(&state, "1", watcher.id());
    assert_eq!(
        running_watchers(&state, &crate::which::SystemProcs),
        vec![Watcher {
            pidfile: pidfile.clone(),
            pid: watcher.id()
        }]
    );

    // A reinstall stops it, so the reload's start-watch.sh finds no live
    // watcher and starts one on the new files.
    assert!(install_at(&paths, &state, true).expect("reinstall"));
    assert!(exited(&mut watcher), "the old watcher stopped");
    assert!(!pidfile.exists(), "its pidfile is gone");
    assert!(state.exists(), "install keeps the state directory");
}

#[cfg(unix)]
#[test]
fn uninstall_stops_the_watcher_and_removes_its_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    assert!(install_at(&paths, &state, true).expect("install"));
    let mut watcher = fake_watcher();
    record(&state, "1", watcher.id());
    std::fs::write(state.join("watch.log"), "log\n").expect("log");

    assert!(uninstall_at(&paths, &state, true).expect("uninstall"));
    assert!(exited(&mut watcher), "the watcher stopped");
    assert!(!state.exists(), "the state directory is gone");
}

#[cfg(unix)]
#[test]
fn uninstall_leaves_a_linked_checkouts_watcher() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = paths_in(tmp.path());
    let state = state_in(tmp.path());
    let checkout = tmp.path().join("checkout");
    std::fs::create_dir_all(&checkout).expect("checkout");
    std::os::unix::fs::symlink(&checkout, &paths.plugin).expect("link");
    std::fs::write(&paths.init, block()).expect("seed init.lua");
    let mut watcher = fake_watcher();
    record(&state, "1", watcher.id());

    assert!(uninstall_at(&paths, &state, true).expect("uninstall"));
    assert!(
        watcher.try_wait().expect("try_wait").is_none(),
        "the checkout's watcher keeps running"
    );
    assert!(state.exists(), "its state stays");
    let _ = watcher.kill();
    let _ = watcher.wait();
}

/// A watcher that outlives the wait keeps its pidfile: the reload's
/// start-watch.sh then finds it live and starts no second one beside it.
#[cfg(unix)]
#[test]
fn a_watcher_that_will_not_stop_keeps_its_pidfile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    let mut stubborn = std::process::Command::new("sh")
        .args([
            "-c",
            "trap '' TERM; while :; do sleep 1; done",
            "tag-watch.lua",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn stubborn watcher");
    let pidfile = record(&state, "1", stubborn.id());
    let watchers = running_watchers(&state, &crate::which::SystemProcs);
    assert_eq!(watchers.len(), 1);

    stop_watchers(
        &watchers,
        &crate::which::SystemProcs,
        Duration::from_millis(300),
    );
    assert!(
        stubborn.try_wait().expect("try_wait").is_none(),
        "it ignored the TERM"
    );
    assert!(pidfile.exists(), "its pidfile stays");
    let _ = stubborn.kill();
    let _ = stubborn.wait();
}

/// The watcher list is read before the prompt. A pid that has since stopped
/// running the watcher (it exited, and the pid went to another program) is
/// never signalled.
#[cfg(unix)]
#[test]
fn a_pid_that_stopped_being_a_watcher_after_the_listing_is_not_signalled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = tmp.path().join("state");
    let mut other = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pidfile = record(&state, "1", other.id());
    let listed = vec![Watcher {
        pidfile: pidfile.clone(),
        pid: other.id(),
    }];

    stop_watchers(
        &listed,
        &crate::which::SystemProcs,
        Duration::from_millis(300),
    );
    assert!(
        other.try_wait().expect("try_wait").is_none(),
        "left running"
    );
    assert!(pidfile.exists(), "its pidfile is left alone");
    let _ = other.kill();
    let _ = other.wait();
}

/// A config load between the stop and the cleanup may record a fresh
/// watcher in the same pidfile: only a pidfile still naming the stopped pid
/// goes.
#[test]
fn only_a_pidfile_naming_the_stopped_watcher_is_removed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pidfile = tmp.path().join("watch-1.pid");
    std::fs::write(&pidfile, "999\n").expect("pidfile");
    remove_pidfile_naming(&pidfile, 123);
    assert!(pidfile.exists(), "a fresh watcher's pidfile stays");
    std::fs::write(&pidfile, "123\n").expect("pidfile");
    remove_pidfile_naming(&pidfile, 123);
    assert!(!pidfile.exists(), "the stopped watcher's pidfile goes");
}

/// A stand-in `rex` CLI in `dir` running `body`.
#[cfg(unix)]
fn fake_rex(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let rex = dir.join("rex");
    std::fs::write(&rex, format!("#!/bin/sh\n{body}")).expect("fake rex");
    std::fs::set_permissions(&rex, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    rex
}

/// A [`Setup`] over a temp config dir, reloading through `rex`.
fn setup_in(dir: &Path, rex: PathBuf) -> Setup {
    Setup {
        paths: paths_in(dir),
        state: state_in(dir),
        rex,
    }
}

/// The commands themselves, past `install_at`/`uninstall_at`: each reloads
/// Rex's config through the setup's CLI once it changed something.
#[cfg(unix)]
#[test]
fn install_and_uninstall_reload_rex() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).expect("bin dir");
    let argv = bin.join("argv");
    let rex = fake_rex(&bin, "printf '%s\\n' \"$@\" >>\"$(dirname \"$0\")/argv\"\n");
    let setup = setup_in(tmp.path(), rex);

    install_with(&setup, true).expect("install");
    assert_eq!(
        std::fs::read_to_string(&argv).expect("rex ran"),
        "config\nreload\n"
    );
    uninstall_with(&setup, true).expect("uninstall");
    assert_eq!(
        std::fs::read_to_string(&argv).expect("rex ran"),
        "config\nreload\nconfig\nreload\n"
    );

    // Nothing left to remove: nothing to reload either.
    uninstall_with(&setup, true).expect("second uninstall");
    assert_eq!(
        std::fs::read_to_string(&argv)
            .expect("rex ran")
            .lines()
            .count(),
        4
    );
}

#[cfg(unix)]
#[test]
fn a_reload_reports_how_the_cli_went() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let timeout = Duration::from_secs(5);
    assert_eq!(
        reload(&fake_rex(tmp.path(), "exit 0\n"), timeout),
        Reload::Done
    );
    assert_eq!(
        reload(
            &fake_rex(tmp.path(), "echo 'no server' >&2\nexit 1\n"),
            timeout
        ),
        Reload::Failed("no server".to_owned())
    );
    assert_eq!(
        reload(&tmp.path().join("missing").join("rex"), timeout),
        Reload::NoCli
    );
}

/// A CLI wedged past its own request limit is killed at the deadline rather
/// than holding install open.
#[cfg(unix)]
#[test]
fn a_reload_that_hangs_is_cut_off() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let rex = fake_rex(tmp.path(), "exec sleep 30\n");
    let started = Instant::now();
    assert_eq!(reload(&rex, Duration::from_millis(200)), Reload::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// The scripts are POSIX sh, so install refuses on Windows before it writes
/// anything.
#[cfg(windows)]
#[test]
fn install_refuses_on_windows() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let setup = setup_in(tmp.path(), PathBuf::from("rex"));
    let err = install_with(&setup, true).expect_err("refused");
    assert!(
        err.to_string().contains("not available on Windows"),
        "{err}"
    );
    assert!(!setup.paths.plugin.exists(), "nothing written");
    assert!(!setup.paths.init.exists(), "init.lua untouched");
}

/// Inside a Rex pane, `REX_BIN_DIR` names the server's own binaries; a dir
/// with no `rex` in it, or none at all, falls back to the one on `PATH`.
#[test]
fn the_reload_cli_comes_from_rex_bin_dir_then_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("rex"), "").expect("rex");
    assert_eq!(
        rex_cli(Some(tmp.path().as_os_str().to_owned())),
        tmp.path().join("rex")
    );
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).expect("empty dir");
    for dir in [Some(empty.into_os_string()), os(""), None] {
        assert_eq!(rex_cli(dir.clone()), PathBuf::from("rex"), "{dir:?}");
    }
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

/// A `clauth` shim in `dir` running `body`.
#[cfg(unix)]
fn clauth_shim(dir: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).expect("bin dir");
    let shim = dir.join("clauth");
    std::fs::write(&shim, format!("#!/bin/sh\n{body}")).expect("shim");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Runs the real `rex-plugin/resolve-profile.sh` for pid 100 with `$HOME` at
/// `home` and the bare `path` Rex hands it. Returns whether the script exited
/// 0, and its stdout.
#[cfg(unix)]
fn run_resolver(home: &Path, path: &str) -> (bool, String) {
    let out = std::process::Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/rex-plugin/resolve-profile.sh"
        ))
        .arg("100")
        .env("HOME", home)
        .env("PATH", path)
        .output()
        .expect("resolve-profile.sh runs");
    (
        out.status.success(),
        String::from_utf8(out.stdout).expect("utf-8"),
    )
}

/// [`run_resolver`] against a sandbox `$HOME` whose `.local/bin/clauth` shim
/// is `clauth_body`, on a system PATH with no clauth: the script must find
/// the shim in the install dirs it appends.
#[cfg(unix)]
fn resolve_with(clauth_body: &str) -> (bool, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    clauth_shim(&tmp.path().join(".local/bin"), clauth_body);
    run_resolver(tmp.path(), "/usr/bin:/bin")
}

/// The install dirs go after the PATH Rex hands the script, so a clauth the
/// server's own PATH names wins over an older one left in `~/.local/bin`.
#[cfg(unix)]
#[test]
fn a_clauth_on_the_inherited_path_wins_over_the_install_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let answer = |profile: &str| {
        format!(
            "printf '%s\\n' '{{\"harness\":\"claude\",\"profile\":\"{profile}\",\"source\":\"global\"}}'\n"
        )
    };
    clauth_shim(&tmp.path().join(".local/bin"), &answer("old"));
    let current = tmp.path().join("current");
    clauth_shim(&current, &answer("new"));

    let (ok, out) = run_resolver(tmp.path(), &format!("{}:/usr/bin:/bin", current.display()));
    assert!(ok);
    assert_eq!(out, "new\tglobal\tclaude\n");
}

/// Runs the real `start-watch.sh` as the child of a stand-in Rex server whose
/// argv is `server_args`, with a stand-in `rex` that records the argv the
/// watcher gets. Returns that argv.
#[cfg(unix)]
fn watcher_argv(server_args: &[&str]) -> Vec<String> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let rex = fake_rex(
        tmp.path(),
        "printf '%s\\n' \"$@\" >\"$(dirname \"$0\")/argv.tmp\"\nmv \"$(dirname \"$0\")/argv.tmp\" \"$(dirname \"$0\")/argv\"\n",
    );
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/rex-plugin/start-watch.sh");
    // `sh -c` with a second command after the script never execs into it, so
    // the script's parent is this sh, whose argv carries the server's flags.
    let status = std::process::Command::new("sh")
        .args(["-c", "sh \"$0\"; wait", script])
        .args(server_args)
        .env("HOME", tmp.path())
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("CLAUTH_REX_BIN", &rex)
        .stdin(std::process::Stdio::null())
        .status()
        .expect("start-watch.sh runs");
    assert!(status.success());
    // The watcher runs in the background: wait for its argv.
    let argv = tmp.path().join("argv");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !argv.exists() {
        assert!(Instant::now() < deadline, "the watcher never started");
        std::thread::sleep(Duration::from_millis(25));
    }
    std::fs::read_to_string(&argv)
        .expect("argv")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The watcher is pinned to whatever `--listen` the server names: macOS's
/// `unix://` URL, Linux's bare shared-server path, or a path with a space in
/// it, in either `--listen X` or `--listen=X` form.
#[cfg(unix)]
#[test]
fn the_watcher_is_pinned_to_any_listen_address() {
    for listen in [
        "unix:///Users/u/Library/Application%20Support/rex/server.sock",
        "/run/rex/server.sock",
        "/tmp/rex dir/server.sock",
    ] {
        for args in [
            vec!["server", "run", "--listen", listen, "--log-format", "text"],
            vec!["server", "run", "--listen", listen],
        ] {
            let argv = watcher_argv(&args);
            assert_eq!(argv[..3], ["--server", listen, "do"], "{args:?}");
        }
        let joined = format!("--listen={listen}");
        let argv = watcher_argv(&["server", "run", &joined, "--max-panes", "64"]);
        assert_eq!(argv[..3], ["--server", listen, "do"], "{joined}");
    }
    // A server with no --listen runs on Rex's default address.
    let argv = watcher_argv(&["server", "run", "--log-format", "text"]);
    assert_eq!(argv[0], "do");
}

/// The script's answer when `clauth which --pid 100 --json` prints `json`.
/// The join itself is the Rust resolver's, pinned by `which::tests`; these
/// pin what the script asks for and how it maps the answer onto the
/// watcher's tab-separated line. Any other argv exits 2, as an unknown flag
/// does.
#[cfg(unix)]
fn resolve(json: &str) -> String {
    let (ok, out) = resolve_with(&format!(
        "[ \"$*\" = 'which --pid 100 --json' ] || exit 2\nprintf '%s\\n' '{json}'\n"
    ));
    assert!(ok, "exits 0 on an answer");
    out
}

#[cfg(unix)]
#[test]
fn a_codex_session_clauth_started_names_the_codex_agent() {
    // `clauth start <codex profile>`: the pane runs the clauth supervisor, so
    // only the row's harness says the agent is codex, not Claude Code.
    let out = resolve(r#"{"harness":"codex","profile":"work","source":"live_session"}"#);
    assert_eq!(out, "work\tclauth\tcodex\n");
}

#[cfg(unix)]
#[test]
fn a_claude_session_clauth_started_names_the_claude_agent() {
    let out = resolve(r#"{"harness":"claude","profile":"work","source":"live_session"}"#);
    assert_eq!(out, "work\tclauth\tclaude\n");
}

#[cfg(unix)]
#[test]
fn a_bare_codex_on_an_adopted_login_names_its_profile() {
    let out = resolve(r#"{"harness":"codex","profile":"work","source":"codex_adopted"}"#);
    assert_eq!(out, "work\tcodex\tcodex\n");
}

#[cfg(unix)]
#[test]
fn a_bare_claude_spends_the_global_login() {
    let out = resolve(r#"{"harness":"claude","profile":"main","source":"global"}"#);
    assert_eq!(out, "main\tglobal\tclaude\n");
}

/// A bare codex on its own login, a `clauth start` whose row is not written
/// yet, a shell: nothing resolves, and the line is empty.
#[cfg(unix)]
#[test]
fn nothing_resolved_prints_nothing() {
    let out = resolve(r#"{"harness":null,"profile":null,"source":null}"#);
    assert_eq!(out, "");
}

#[cfg(unix)]
#[test]
fn an_answer_the_watcher_cannot_label_prints_nothing() {
    for json in [
        r#"{"harness":"cursor","profile":"work","source":"live_session"}"#,
        r#"{"harness":"claude","profile":"work","source":"somewhere_new"}"#,
        r#"{"harness":"claude","profile":"","source":"global"}"#,
    ] {
        assert_eq!(resolve(json), "", "{json}");
    }
}

/// A clauth that predates `which --pid` exits 2 on the flag. That is no
/// answer, so the script fails and the watcher keeps the pane's label.
#[cfg(unix)]
#[test]
fn a_clauth_without_which_pid_fails_the_script() {
    let (ok, out) = resolve_with("exit 2\n");
    assert!(!ok, "exits nonzero");
    assert_eq!(out, "");
}
