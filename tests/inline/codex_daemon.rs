#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::path::PathBuf;
use std::time::Duration;

#[test]
fn a_login_moved_after_the_daemon_started_is_newer() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    assert!(login_is_newer(t0, Some(t0 + Duration::from_millis(1))));
    assert!(!login_is_newer(t0, Some(t0)));
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
/// codex's own managed binary when installed, else `codex` from PATH.
#[test]
fn the_restart_runs_codexs_managed_binary_else_path() {
    let home = tempfile::tempdir().unwrap();
    let args = |c: &Command| {
        c.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };

    let bare = restart_command(home.path());
    assert_eq!(bare.get_program(), "codex");
    assert_eq!(args(&bare), ["app-server", "daemon", "restart"]);

    let managed = home.path().join(MANAGED_BIN);
    std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
    std::fs::write(&managed, "").unwrap();
    let cmd = restart_command(home.path());
    assert_eq!(cmd.get_program(), managed.as_os_str());
    assert_eq!(args(&cmd), ["app-server", "daemon", "restart"]);
}

/// The CLI note reads the operator's own codex home: present only while a
/// daemon there predates the login's last move.
#[test]
fn the_switch_note_follows_the_operators_daemon() {
    let home = crate::testutil::HomeSandbox::new();
    assert_eq!(switch_note(), None, "no daemon, no note");

    let operator = home.home().join(".codex");
    std::fs::create_dir_all(operator.join("app-server-control")).unwrap();
    let (_l, sock) = listener("note");
    std::os::unix::fs::symlink(&sock, operator.join(CONTROL_SOCKET)).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    std::os::unix::fs::symlink("profiles/x/auth.json", operator.join("auth.json")).unwrap();

    let note = switch_note().expect("a daemon older than the login");
    assert!(note.contains("codex app-server daemon restart"), "{note}");
    let _ = std::fs::remove_file(sock);
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
    assert!(
        restart_log_line("auto", &ok, limit).contains("restarted codex's app-server daemon (auto)")
    );
    let failed = Waited::Exited {
        ok: false,
        status: "exit status: 3".into(),
    };
    assert!(restart_log_line("auto", &failed, limit).ends_with("exited exit status: 3"));
    assert!(
        restart_log_line("auto", &Waited::Killed, limit).contains("ran past 60s and was killed")
    );
    assert!(
        restart_log_line("auto", &Waited::Unwaitable("EINTR".into()), limit)
            .contains("could not be waited on: EINTR")
    );
}
