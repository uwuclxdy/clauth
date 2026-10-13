#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::path::PathBuf;
use std::time::Duration;

#[test]
fn a_login_moved_after_the_daemon_started_is_newer() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    assert!(login_is_newer(t0, Some(t0 + Duration::from_millis(1))));
    // One timestamp tick for both reads as moved: a needless restart costs a
    // resumable turn, a missed one strands every task on the old account.
    assert!(login_is_newer(t0, Some(t0)));
    assert!(!login_is_newer(t0, Some(t0 - Duration::from_secs(60))));
    assert!(!login_is_newer(t0, None));
}

/// A codex home with the daemon's control link and the operator login link,
/// created in `order`, the control link pointing at `target`.
fn home_with(order: &[&str], target: &Path) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("app-server-control")).unwrap();
    for which in order {
        match *which {
            "daemon" => {
                std::os::unix::fs::symlink(target, home.path().join(CONTROL_SOCKET)).unwrap()
            }
            "login" => {
                std::os::unix::fs::symlink("profiles/x/auth.json", home.path().join("auth.json"))
                    .unwrap()
            }
            _ => unreachable!(),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    home
}

/// A listening socket at a short path (sun_path caps at 104 bytes on macOS).
fn listener(tag: &str) -> (std::os::unix::net::UnixListener, PathBuf) {
    let path = PathBuf::from(format!("/tmp/clauth-cdx-{}-{tag}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    (std::os::unix::net::UnixListener::bind(&path).unwrap(), path)
}

#[test]
fn a_daemon_started_before_the_switch_is_stale() {
    let (_l, sock) = listener("before");
    let home = home_with(&["daemon", "login"], &sock);
    assert!(is_stale_at(home.path()));
    let _ = std::fs::remove_file(sock);
}

#[test]
fn a_daemon_started_after_the_switch_is_current() {
    let (_l, sock) = listener("after");
    let home = home_with(&["login", "daemon"], &sock);
    assert!(!is_stale_at(home.path()));
    let _ = std::fs::remove_file(sock);
}

/// A crashed daemon leaves its link behind; nothing answers, so no banner.
#[test]
fn a_leftover_link_with_no_daemon_is_not_reported() {
    let gone = PathBuf::from(format!("/tmp/clauth-cdx-{}-gone.sock", std::process::id()));
    let home = home_with(&["daemon", "login"], &gone);
    assert!(!is_stale_at(home.path()));
}

#[test]
fn no_daemon_no_answer() {
    let home = tempfile::tempdir().unwrap();
    assert!(!is_stale_at(home.path()));
}

/// A home deep enough that the control link's own path is past sun_path's
/// 104 bytes: the check connects through the link's target, as codex does.
#[test]
fn a_long_home_still_finds_its_daemon() {
    let (_l, sock) = listener("long");
    let base = tempfile::tempdir().unwrap();
    let deep = base
        .path()
        .join("a-rather-long-directory-name-to-push-the-path")
        .join("past-the-limit");
    std::fs::create_dir_all(deep.join("app-server-control")).unwrap();
    std::os::unix::fs::symlink(&sock, deep.join(CONTROL_SOCKET)).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    std::os::unix::fs::symlink("profiles/x/auth.json", deep.join("auth.json")).unwrap();
    assert!(deep.join(CONTROL_SOCKET).as_os_str().len() > 104);
    assert!(is_stale_at(&deep));
    let _ = std::fs::remove_file(sock);
}

/// Building the restart spawns nothing, so its shape is checked directly:
/// codex's own managed binary when installed, else `codex` from PATH, and
/// `CODEX_HOME` pinned to the home judged stale, since codex finds its daemon
/// from its own `CODEX_HOME`.
#[test]
fn the_restart_runs_codexs_managed_binary_else_path() {
    let home = tempfile::tempdir().unwrap();
    let args = |c: &Command| {
        c.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };

    let codex_home = |c: &Command| {
        c.get_envs()
            .find(|(k, _)| *k == "CODEX_HOME")
            .and_then(|(_, v)| v)
            .map(std::ffi::OsStr::to_os_string)
    };

    let bare = restart_command(home.path());
    assert_eq!(bare.get_program(), "codex");
    assert_eq!(args(&bare), ["app-server", "daemon", "restart"]);
    assert_eq!(codex_home(&bare).as_deref(), Some(home.path().as_os_str()));

    let managed = home.path().join(MANAGED_BIN);
    std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
    std::fs::write(&managed, "").unwrap();
    let cmd = restart_command(home.path());
    assert_eq!(cmd.get_program(), managed.as_os_str());
    assert_eq!(args(&cmd), ["app-server", "daemon", "restart"]);
    assert_eq!(codex_home(&cmd).as_deref(), Some(home.path().as_os_str()));
}

/// The CLI note reads the codex home whose login the switch moved: present
/// only while a daemon there predates that move.
#[test]
fn the_switch_note_follows_the_moved_homes_daemon() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(switch_note(home.path()), None, "no daemon, no note");

    std::fs::create_dir_all(home.path().join("app-server-control")).unwrap();
    let (_l, sock) = listener("note");
    std::os::unix::fs::symlink(&sock, home.path().join(CONTROL_SOCKET)).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    std::os::unix::fs::symlink("profiles/x/auth.json", home.path().join("auth.json")).unwrap();

    assert_eq!(
        switch_note(home.path()),
        Some(
            "clauth: codex's app-server daemon still holds the previous login, so running and \
             resumed codex tasks keep using it. Run `codex app-server daemon restart` to apply \
             the switch (running tasks stop their current turn and stay resumable)."
        )
    );
    let _ = std::fs::remove_file(sock);
}

/// A restart runs only while the daemon still predates the login, one at a
/// time: one queued behind another finds the daemon reloaded and skips, so two
/// switches inside one restart's window never run two restarts at once.
#[test]
fn restarts_run_one_at_a_time_and_a_queued_one_skips_a_reloaded_daemon() {
    let (_l, sock) = listener("serial");
    let current = home_with(&["login", "daemon"], &sock);
    assert_eq!(restart_serialized(current.path(), || "ran"), None);

    let home = home_with(&["daemon", "login"], &sock);
    let path = home.path().to_path_buf();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let first = std::thread::spawn({
        let path = path.clone();
        move || {
            restart_serialized(&path, || {
                held_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                // The restarted daemon recreates its control link.
                let link = path.join(CONTROL_SOCKET);
                std::fs::remove_file(&link).unwrap();
                std::thread::sleep(Duration::from_millis(20));
                std::os::unix::fs::symlink(&sock, &link).unwrap();
                "first"
            })
        }
    });
    held_rx.recv().unwrap();
    assert_eq!(restart_serialized(&path, || "second"), None);
    assert_eq!(first.join().unwrap(), Some("first"));
    let _ = std::fs::remove_file(path.join(CONTROL_SOCKET));
}

/// A hung `codex` must not keep the restart thread forever: the waiter kills a
/// child past its deadline, and reports a clean exit and a failed one apart.
#[test]
fn the_restart_wait_is_bounded_and_reports_each_outcome() {
    let spawn = |cmd: &str| std::process::Command::new(cmd).spawn().unwrap();
    let limit = Duration::from_secs(5);
    assert!(matches!(
        wait_with_deadline(&mut spawn("true"), limit),
        Waited::Exited { ok: true, .. }
    ));
    assert!(matches!(
        wait_with_deadline(&mut spawn("false"), limit),
        Waited::Exited { ok: false, .. }
    ));
    let mut hung = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let began = std::time::Instant::now();
    assert_eq!(
        wait_with_deadline(&mut hung, Duration::from_millis(200)),
        Waited::Killed
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "killed at the deadline, not waited out"
    );
}

#[test]
fn each_restart_outcome_reads_as_what_happened() {
    let limit = Duration::from_secs(60);
    let ok = Waited::Exited {
        ok: true,
        status: "exit status: 0".into(),
    };
    assert_eq!(
        restart_log_line("auto", &ok, limit),
        "clauth: restarted codex's app-server daemon (auto) so it loads the active login"
    );
    let failed = Waited::Exited {
        ok: false,
        status: "exit status: 3".into(),
    };
    assert_eq!(
        restart_log_line("auto", &failed, limit),
        "clauth: codex app-server restart (auto) failed with exit status: 3"
    );
    assert_eq!(
        restart_log_line("auto", &Waited::Killed, limit),
        "clauth: codex app-server restart (auto) ran past 60s and was killed"
    );
    assert_eq!(
        restart_log_line("auto", &Waited::Unwaitable("EINTR".into()), limit),
        "clauth: codex app-server restart (auto) could not be waited on: EINTR"
    );
}
