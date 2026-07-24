//! Timestamped log lines for daemon-visible events, routed off any interactive
//! console.
//!
//! One entry point ([`logline!`]), three sinks picked from the process context:
//!
//! - **daemon** ([`enable_timestamps`] flipped once at `serve()`): stderr, which
//!   the supervisor redirects to `daemon.log`. ISO-8601-UTC stamped.
//! - **interactive TUI / CLI on a terminal**: `~/.clauth/clauth.log`. Here stderr
//!   IS the ratatui alternate screen, so a bare line from a background scheduler
//!   thread paints straight over the accounts pane (the 2026-07-14 corruption
//!   report). The line is stamped and diverted to the log file instead.
//! - **piped / redirected stderr** (CI, `2>file`): stderr, bare — the caller
//!   already chose where those bytes land.
//!
//! `daemon.log` lines used to carry no timestamps — the 2026-07-09 broken-active
//! incident had to be reconstructed by guessing switch/quarantine ordering from
//! bare lines. Every daemon-reachable event now dates itself.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::daemon::log_rotate;

static STAMP: AtomicBool = AtomicBool::new(false);
static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Turn on timestamp prefixes for [`logline!`] — called once at the top of
/// `daemon::serve()` (lock-holder, standing-by, and a redundant instance that
/// exits immediately all say so on stderr, TTY or not) and of `proxy::run()`
/// (its stderr is the supervised `proxy.log`).
/// Sticky for the process lifetime; never flipped back.
pub(crate) fn enable_timestamps() {
    STAMP.store(true, Ordering::Relaxed);
}

/// One line as it will hit its sink — split from [`line`] so the format is
/// pinned by a unit test without capturing stderr.
pub(crate) fn render(stamped: bool, now_secs: i64, msg: &str) -> String {
    if stamped {
        format!("{} {msg}", crate::usage::epoch_secs_to_iso(now_secs))
    } else {
        msg.to_string()
    }
}

/// Where a rendered line goes. The daemon always writes stderr (its redirected
/// log); an interactive stderr that IS a terminal diverts to the log file so a
/// background thread's line can never paint over the TUI's alternate screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    Stderr,
    LogFile,
}

fn route(stamped: bool, stderr_is_tty: bool) -> Sink {
    if !stamped && stderr_is_tty {
        Sink::LogFile
    } else {
        Sink::Stderr
    }
}

/// `logline!` backend — call the macro, not this.
pub(crate) fn line(args: std::fmt::Arguments<'_>) {
    let raw = args.to_string();
    let now = crate::usage::now_epoch_secs();
    let stamped = STAMP.load(Ordering::Relaxed);
    match route(stamped, std::io::stderr().is_terminal()) {
        Sink::Stderr => eprintln!("{}", render(stamped, now, &raw)),
        // Always stamp in the file — a bare diagnostic log is useless for the
        // forensics this exists for.
        Sink::LogFile => append_logfile(&render(true, now, &raw)),
    }
}

/// `~/.clauth/clauth.log`, resolved and size-capped once per process. `None`
/// when the clauth dir can't be resolved — the line is then dropped, since a
/// diagnostic log must never take down its caller.
fn log_path() -> Option<&'static Path> {
    LOG_PATH
        .get_or_init(|| {
            let path = crate::profile::clauth_dir().ok()?.join("clauth.log");
            // Trim once at first use. Event lines are sparse, so within-session
            // growth is negligible; add a per-write trim if a hot logger lands here.
            let _ = log_rotate::rotate_log_if_large(
                &path,
                log_rotate::LOG_MAX_BYTES,
                log_rotate::LOG_KEEP_BYTES,
            );
            Some(path)
        })
        .as_deref()
}

fn append_logfile(rendered: &str) {
    if let Some(path) = log_path() {
        write_log_line(path, rendered);
    }
}

/// Append one line, re-opening each call (event lines are rare, so no held fd to
/// coordinate). Best-effort: an unwritable log never propagates back to the
/// event source.
fn write_log_line(path: &Path, rendered: &str) {
    // 0o600 on create: an event line names profiles, endpoints, and failure
    // bodies, and the log lives under `~/.clauth` — owner-only like the rest.
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(path) {
        let _ = writeln!(f, "{rendered}");
    }
}

/// One event line: stderr for the daemon (ISO-8601-UTC prefixed once
/// [`enable_timestamps`] is set), else `~/.clauth/clauth.log` on an interactive
/// terminal so it never paints over the TUI.
macro_rules! logline {
    ($($arg:tt)*) => {
        $crate::logline::line(::std::format_args!($($arg)*))
    };
}
pub(crate) use logline;

#[cfg(test)]
#[path = "../tests/inline/logline.rs"]
mod tests;
