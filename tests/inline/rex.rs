#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::BTreeMap;

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
            version: Some("0.17.0".to_owned()),
        })
    );
}

#[test]
fn a_marker_whose_first_line_is_not_the_version_reads_none() {
    for marker in [
        "",
        "garbage\nwritten by clauth 0.17.0\n",
        "written by clauth \n",
    ] {
        let fx = RexFixture::new();
        fx.managed(marker);
        assert_eq!(
            fx.status(&FakeProcs::new(&[])).map(|s| s.plugin),
            Some(PluginDir::Managed { version: None }),
            "marker {marker:?}"
        );
    }
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
        })
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
