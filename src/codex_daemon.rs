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
//! that mismatch instead, until it restarts. No reload message exists that
//! could replace the restart.
//!
//! The daemon recreates its control-socket link at every start and removes it
//! on a clean exit, so that link's mtime is when it started, and the operator
//! slot's link mtime is when a switch last repointed it.

use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

/// The daemon's control socket, relative to the operator's codex home.
const CONTROL_SOCKET: &str = "app-server-control/app-server-control.sock";
/// The daemon binary codex manages for itself, relative to the codex home.
const MANAGED_BIN: &str = "packages/app-server-daemon/current/bin/codex";
/// How long a restart may run before its child is killed and the attempt is
/// logged as hung.
#[cfg(not(test))]
const RESTART_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// A daemon runs under the operator's codex home and started before the
/// operator's login last moved.
pub(crate) fn is_stale() -> bool {
    crate::actions::default_codex_operator_home().is_ok_and(|home| is_stale_at(&home))
}

/// [`is_stale`] for an explicit codex home. File stats only, until the answer
/// would be "stale": only then is the socket connected to, which rules out a
/// crashed daemon's leftover link, and a caller asking when nothing moved
/// never touches the daemon.
pub(crate) fn is_stale_at(home: &Path) -> bool {
    let socket = home.join(CONTROL_SOCKET);
    let Some(started) = std::fs::symlink_metadata(&socket)
        .ok()
        .and_then(|m| m.modified().ok())
    else {
        return false;
    };
    let login_moved = std::fs::symlink_metadata(home.join("auth.json"))
        .ok()
        .and_then(|m| m.modified().ok());
    if !login_is_newer(started, login_moved) {
        return false;
    }
    answers(&socket)
}

/// Something listens behind the control link. Connected through the link's
/// target: codex keeps the socket under a fixed short root and links to it
/// from the codex home, and a long home can put the link path itself past
/// sun_path's 104 bytes on macOS (our reading of why the direct connect
/// failed under a deep sandbox home; codex's own stated reason for the fixed
/// root is that it must not depend on HOME, TMPDIR or CODEX_HOME).
#[cfg(unix)]
fn answers(socket: &Path) -> bool {
    std::fs::canonicalize(socket)
        .and_then(std::os::unix::net::UnixStream::connect)
        .is_ok()
}

#[cfg(not(unix))]
fn answers(_socket: &Path) -> bool {
    false
}

/// True when the operator's login moved after the daemon started. Pure, so the
/// ordering rule is tested without a daemon.
pub(crate) fn login_is_newer(daemon_started: SystemTime, login_moved: Option<SystemTime>) -> bool {
    login_moved.is_some_and(|moved| moved > daemon_started)
}

/// The command that restarts the daemon: codex's own managed binary when it is
/// installed, else `codex` from PATH. Building it spawns nothing.
pub(crate) fn restart_command(home: &Path) -> Command {
    let managed = home.join(MANAGED_BIN);
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
/// the log line. Under test it only counts, per thread, so no test can restart
/// a real daemon and a test's count is its own.
pub(crate) fn restart_in_background(why: &'static str) {
    #[cfg(test)]
    {
        let _ = why;
        TEST_RESTARTS.with(|n| n.set(n.get() + 1));
    }
    #[cfg(not(test))]
    restart_now(why);
}

#[cfg(test)]
thread_local! {
    /// Restarts [`restart_in_background`] was asked for on this thread.
    pub(crate) static TEST_RESTARTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How a restart child ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Waited {
    /// It exited; `ok` is a zero status, `status` its display form.
    Exited { ok: bool, status: String },
    /// It ran past the deadline and was killed.
    Killed,
    /// It could not be waited on.
    Unwaitable(String),
}

/// Wait for `child`, killing it once `deadline` passes, so a hung `codex`
/// never keeps the restart thread forever.
pub(crate) fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: std::time::Duration,
) -> Waited {
    let until = std::time::Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Waited::Exited {
                    ok: status.success(),
                    status: status.to_string(),
                };
            }
            Ok(None) if std::time::Instant::now() >= until => {
                let _ = child.kill();
                let _ = child.wait();
                return Waited::Killed;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => return Waited::Unwaitable(e.to_string()),
        }
    }
}

/// The daemon-log line for a restart's outcome. Pure, so the wording is tested.
pub(crate) fn restart_log_line(
    why: &str,
    waited: &Waited,
    deadline: std::time::Duration,
) -> String {
    match waited {
        Waited::Exited { ok: true, .. } => {
            format!(
                "clauth: restarted codex's app-server daemon ({why}) so it loads the active login"
            )
        }
        Waited::Exited { status, .. } => {
            format!("clauth: codex app-server restart ({why}) exited {status}")
        }
        Waited::Killed => format!(
            "clauth: codex app-server restart ({why}) ran past {}s and was killed",
            deadline.as_secs()
        ),
        Waited::Unwaitable(e) => {
            format!("clauth: codex app-server restart ({why}) could not be waited on: {e}")
        }
    }
}

#[cfg(not(test))]
fn restart_now(why: &'static str) {
    use crate::logline::logline;
    let Ok(home) = crate::actions::default_codex_operator_home() else {
        logline!("clauth: codex app-server restart ({why}) skipped: no home directory");
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("clauth-codex-daemon-restart".into())
        .spawn(move || {
            let spawned = restart_command(&home)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match spawned {
                Ok(mut child) => {
                    let waited = wait_with_deadline(&mut child, RESTART_DEADLINE);
                    logline!("{}", restart_log_line(why, &waited, RESTART_DEADLINE));
                }
                Err(e) => logline!("clauth: codex app-server restart ({why}) could not run: {e}"),
            }
        });
    if let Err(e) = spawned {
        logline!("clauth: codex app-server restart ({why}) thread failed to spawn: {e}");
    }
}

/// The CLI's note after a codex switch while a stale daemon runs.
pub(crate) fn switch_note() -> Option<&'static str> {
    is_stale().then_some(
        "clauth: codex's app-server daemon still holds the previous login, so running and \
         resumed codex tasks keep using it. Run `codex app-server daemon restart` to apply \
         the switch (running tasks stop their current turn and stay resumable).",
    )
}

#[cfg(all(test, unix))]
#[path = "../tests/inline/codex_daemon.rs"]
mod tests;
