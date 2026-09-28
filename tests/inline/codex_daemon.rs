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
    assert!(stale_daemon_at(home.path()).is_some());
    let _ = std::fs::remove_file(sock);
}

#[test]
fn a_daemon_started_after_the_switch_is_current() {
    let (_l, sock) = listener("after");
    let home = home_with(&["login", "daemon"], &sock);
    assert_eq!(stale_daemon_at(home.path()), None);
    let _ = std::fs::remove_file(sock);
}

/// A crashed daemon leaves its link behind; nothing answers, so no banner.
#[test]
fn a_leftover_link_with_no_daemon_is_not_reported() {
    let gone = PathBuf::from(format!("/tmp/clauth-cdx-{}-gone.sock", std::process::id()));
    let home = home_with(&["daemon", "login"], &gone);
    assert_eq!(stale_daemon_at(home.path()), None);
}

#[test]
fn no_daemon_no_answer() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(stale_daemon_at(home.path()), None);
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
    assert!(stale_daemon_at(&deep).is_some());
    let _ = std::fs::remove_file(sock);
}
