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
/// `daemon::serve()`, so the lock-holder, the standing-by instance, and a
/// redundant one that exits immediately all say so on stderr, TTY or not.
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
///
/// **A diagnostic reached once per CALL from a hook or an MCP process must take
/// [`to_logfile`], never [`line`].** [`route`] sends a process whose stderr is
/// not a terminal and which has not enabled stamping — exactly a hook, exactly
/// `clauth mcp` — to [`Sink::Stderr`], where nothing bounds the total. A
/// per-EVENT line there is fine and every current caller is one; a per-CALL one
/// floods the channel Claude Code surfaces to the user, which took a review to
/// catch once already. [`Sink::LogFile`] is size-rotated.
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
    #[cfg(test)]
    if captured(&raw) {
        return;
    }
    let now = crate::usage::now_epoch_secs();
    let stamped = STAMP.load(Ordering::Relaxed);
    match route(stamped, std::io::stderr().is_terminal()) {
        Sink::Stderr => write_stderr_line(&render(stamped, now, &raw)),
        // Always stamp in the file — a bare diagnostic log is useless for the
        // forensics this exists for.
        Sink::LogFile => append_logfile(&render(true, now, &raw)),
    }
}

/// Always the log FILE, never stderr, whatever [`route`] would pick.
///
/// For a caller that runs once per tool call in a hook process. A hook's stderr
/// is a pipe rather than a terminal and `STAMP` is off, which is precisely the
/// `Sink::Stderr` arm — so a per-fire diagnostic through [`line`] is an
/// unbounded write onto the channel Claude Code surfaces to the user, and a
/// persistent failure floods it exactly like the bug it was reporting. The file
/// is bounded by `rotate_log_if_large`; stderr is not.
pub(crate) fn to_logfile(args: std::fmt::Arguments<'_>) {
    let raw = args.to_string();
    #[cfg(test)]
    if captured(&raw) {
        return;
    }
    append_logfile(&render(true, crate::usage::now_epoch_secs(), &raw));
}

/// Best-effort, on the same terms as [`write_log_line`]: the caller is usually
/// a background scheduler or watchdog thread, and an event line that cannot
/// reach its sink must never end it. `eprintln!` did exactly that, and the
/// sink this arm writes is the daemon's redirected `daemon.log`, where a closed
/// pipe is unreachable and a full disk is the error that actually shows up.
///
/// `out::errln!` is the strict form and belongs to the foreground CLI, where a
/// write failure that is not a departed reader means the run has lost its only
/// channel to the operator. Nothing on a background thread may reach for it.
fn write_stderr_line(rendered: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{rendered}");
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

// ── test-only capture ────────────────────────────────────────────────────────
//
// Diverts the lines one thread raises into a buffer, so a test can assert that
// a loop REACHED a call site rather than only that the site formats what it
// claims. [`render`] splits the format out for a pure assertion; nothing before
// this could say whether anything ever called it.
//
// Per-thread rather than a process-global, because under `cargo.sh`'s
// `cargo test` fallback every `tests/inline/*.rs` compiles into one binary whose
// tests are THREADS (nextest gives each its own process; the fallback does not).
// A global buffer would hand one test its neighbour's lines and take those lines
// away from the neighbour's own assertions. The emitting thread is the unit a
// test can name — it is the one the test spawned.

#[cfg(test)]
thread_local! {
    static CAPTURE: std::cell::RefCell<Option<LogLines>> =
        const { std::cell::RefCell::new(None) };
}

/// A capture buffer. Clone it into the thread whose lines are wanted, keep the
/// original to read them back.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct LogLines(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

#[cfg(test)]
impl LogLines {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Take every line raised on the CALLING thread until the returned guard
    /// drops. Panics when one is already installed: the inner guard's drop would
    /// silently retire the outer capture, and a line vanishing is the one
    /// failure a capture harness cannot report on itself.
    #[must_use]
    pub(crate) fn capture_here(&self) -> LogCapture {
        CAPTURE.with(|c| {
            let mut c = c.borrow_mut();
            assert!(
                c.is_none(),
                "a logline capture is already installed on this thread"
            );
            *c = Some(self.clone());
        });
        LogCapture(std::marker::PhantomData)
    }

    /// The lines taken so far, oldest first.
    pub(crate) fn snapshot(&self) -> Vec<String> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// RAII half of [`LogLines::capture_here`]: restores the thread's real sink on
/// drop, panic included.
///
/// `!Send` by construction. A guard is a claim on the thread that installed it,
/// so one carried to another thread would clear that thread's (empty) slot and
/// leave the installing thread capturing for the rest of the process — which
/// presents as a test somewhere else losing its lines.
#[cfg(test)]
pub(crate) struct LogCapture(std::marker::PhantomData<*const ()>);

#[cfg(test)]
impl Drop for LogCapture {
    fn drop(&mut self) {
        // `try_with`, so a drop running during thread teardown restores nothing
        // instead of panicking the thread it exists to report on.
        let _ = CAPTURE.try_with(|c| c.borrow_mut().take());
    }
}

/// Whether this thread's capture took `raw`.
#[cfg(test)]
fn captured(raw: &str) -> bool {
    CAPTURE
        .try_with(|c| {
            c.borrow()
                .as_ref()
                .map(|lines| {
                    lines
                        .0
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(raw.to_string());
                })
                .is_some()
        })
        .unwrap_or(false)
}

#[cfg(test)]
#[path = "../tests/inline/logline.rs"]
mod tests;
