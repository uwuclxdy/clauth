//! Codex's shared app-server daemon, and when it still holds a login the
//! operator switched away from.
//!
//! Since codex 0.157 a `codex` TUI is a client of one long-lived app-server
//! daemon (`codex app-server --managed-daemon`); every task runs inside it and
//! `codex resume` reconnects to the same process. The daemon reads
//! `~/.codex/auth.json` once, when it starts. A clauth codex switch repoints
//! that file, so it reaches only a daemon started after the switch: until the
//! daemon restarts, every task keeps spending the previous account (observed
//! 2026-09-27: a resumed task stayed on a week-spent account after a switch).
//! It does NOT write the old login into the new account's store: codex saves
//! `auth.json` through the link, but every refresh path first re-reads it and
//! refuses on an account-id mismatch (0.157.1 `refresh_token` /
//! `UnauthorizedRecovery`; checked in a sandbox with forged tokens and a local
//! refresh server, 2026-09-28). Once its access token expires it errors with
//! that mismatch instead, until it restarts.
//!
//! The daemon recreates its control-socket link at every start, so that link's
//! mtime is when it started, and the operator slot's link mtime is when a
//! switch last repointed it.

use std::path::Path;
#[cfg(not(test))]
use std::path::PathBuf;
#[cfg(not(test))]
use std::process::Command;
use std::time::SystemTime;

#[cfg(not(test))]
use crate::logline::logline;

/// The daemon's control socket, relative to the operator's codex home.
const CONTROL_SOCKET: &str = "app-server-control/app-server-control.sock";
/// The daemon binary codex manages for itself, relative to the codex home.
#[cfg(not(test))]
const MANAGED_BIN: &str = "packages/app-server-daemon/current/bin/codex";

/// A running daemon that started before the operator's login last moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StaleDaemon {
    pub(crate) started: SystemTime,
}

/// The stale daemon under the operator's codex home, if there is one.
pub(crate) fn stale_daemon() -> Option<StaleDaemon> {
    let home = crate::actions::default_codex_operator_home().ok()?;
    stale_daemon_at(&home)
}

/// [`stale_daemon`] for an explicit codex home. File stats only, until the
/// answer would be "stale": that is the one case a caller acts on, so only then
/// is the socket connected to, which rules out a crashed daemon's leftover link,
/// and a caller asking when nothing moved never touches the daemon.
pub(crate) fn stale_daemon_at(home: &Path) -> Option<StaleDaemon> {
    let socket = home.join(CONTROL_SOCKET);
    let started = std::fs::symlink_metadata(&socket).ok()?.modified().ok()?;
    let login_moved = std::fs::symlink_metadata(home.join("auth.json"))
        .ok()
        .and_then(|m| m.modified().ok());
    if !login_is_newer(started, login_moved) {
        return None;
    }
    // Through the link's target: sun_path caps at 104 bytes on macOS, which
    // is why codex keeps the socket at a short /tmp path behind this link, and
    // a long home puts the link path itself over it.
    #[cfg(unix)]
    std::os::unix::net::UnixStream::connect(std::fs::canonicalize(&socket).ok()?).ok()?;
    Some(StaleDaemon { started })
}

/// True when the operator's login moved after the daemon started. Pure, so the
/// ordering rule is tested without a daemon.
pub(crate) fn login_is_newer(daemon_started: SystemTime, login_moved: Option<SystemTime>) -> bool {
    login_moved.is_some_and(|moved| moved > daemon_started)
}

/// The command that restarts the daemon: codex's own managed binary when it is
/// installed, else `codex` from PATH.
#[cfg(not(test))]
fn restart_command(home: &Path) -> Command {
    let managed: PathBuf = home.join(MANAGED_BIN);
    let program = if managed.is_file() {
        managed.into_os_string()
    } else {
        "codex".into()
    };
    let mut cmd = Command::new(program);
    cmd.args(["app-server", "daemon", "restart"]);
    cmd
}

/// Restart the daemon on a thread of its own and log the outcome. Running
/// tasks lose their current turn and stay resumable. `why` names the trigger in
/// the log line.
pub(crate) fn restart_in_background(why: &'static str) {
    // A test must never restart the operator's real daemon: it only counts.
    #[cfg(test)]
    {
        let _ = why;
        TEST_RESTARTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    #[cfg(not(test))]
    restart_now(why);
}

/// Restarts [`restart_in_background`] was asked for in this test process.
#[cfg(test)]
pub(crate) static TEST_RESTARTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(not(test))]
fn restart_now(why: &'static str) {
    let Ok(home) = crate::actions::default_codex_operator_home() else {
        logline!("clauth: codex app-server restart ({why}) skipped: no home directory");
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("clauth-codex-daemon-restart".into())
        .spawn(move || match restart_command(&home).output() {
            Ok(out) if out.status.success() => {
                logline!("clauth: restarted codex's app-server daemon ({why}) so it loads the active login");
            }
            Ok(out) => logline!(
                "clauth: codex app-server restart ({why}) exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => logline!("clauth: codex app-server restart ({why}) could not run: {e}"),
        });
    if let Err(e) = spawned {
        logline!("clauth: codex app-server restart ({why}) thread failed to spawn: {e}");
    }
}

/// The CLI's note after a codex switch while a stale daemon runs.
pub(crate) fn switch_note() -> Option<&'static str> {
    stale_daemon().map(|_| {
        "clauth: codex's app-server daemon still holds the previous login, so running and \
         resumed codex tasks keep using it. Run `codex app-server daemon restart` to apply \
         the switch (running tasks stop their current turn and stay resumable)."
    })
}

#[cfg(all(test, unix))]
#[path = "../tests/inline/codex_daemon.rs"]
mod tests;
