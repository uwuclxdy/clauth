//! Stdout that survives the reader leaving.
//!
//! Rust ignores `SIGPIPE`, so a write into a pipe whose reader is gone comes
//! back `EPIPE` and `println!` panics on it — `clauth sessions | head -3`
//! printed a Rust panic and exited 101 where a CLI should just stop. Restoring
//! the default disposition once at startup is the usual fix and the wrong one
//! for this binary: `std` sets no `MSG_NOSIGNAL` on Linux socket writes, so it
//! would also kill the daemon the first time an upstream hung up mid-request.
//! The handling lives at the emitter instead.
//!
//! [`outln!`] and [`out!`] are what the crate prints with, [`errln!`] is the
//! stderr half; a bare `println!` or `eprintln!` under `src/` is the bug this
//! module exists to stop, and a guard test fails on one.
//! `platform::copy_to_clipboard_osc52` is the one sanctioned raw stdout
//! writer, for the OSC 52 clipboard escape, which is a terminal command rather
//! than a line of output.
//!
//! The two halves answer a closed reader differently. Stdout carries the
//! payload, so a gone reader ends the run at exit 0. Stderr carries diagnostics
//! and the fatal-error line, so a gone reader drops the line and the run
//! continues to its own exit code — exiting there would let a closed stderr
//! take down a whole `clauth start` session over a background `logline!`.
//!
//! [`errln!`] is for the FOREGROUND CLI, and keeps the stdout half's strictness
//! about every other write error: a run that cannot write its own error message
//! has lost its only channel to the operator. A background thread has no such
//! channel to lose, so `logline!`'s stderr sink writes best-effort on its own
//! (`logline::write_stderr_line`) rather than panicking a scheduler thread over
//! a full disk. Nothing on a background thread reaches for [`errln!`].

use std::fmt::Arguments;
use std::io::{ErrorKind, Write};

/// Whether a chunk reached its reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wrote {
    /// The bytes landed.
    Yes,
    /// The reader closed the pipe. Nothing written after this can land either.
    ReaderGone,
}

/// Write one chunk, flushing when it carries no newline of its own, and hand
/// every outcome back to the caller: [`Wrote::Yes`], [`Wrote::ReaderGone`] for
/// an `EPIPE`, or the write error itself. Never panics, so a caller that minted
/// something it must roll back sees the error as a value it can act on first.
pub(crate) fn write_chunk_result<W: Write>(
    w: &mut W,
    args: Arguments<'_>,
    newline: bool,
) -> Result<Wrote, std::io::Error> {
    let written = if newline {
        writeln!(w, "{args}")
    } else {
        // No newline means a prompt, and line buffering would hold it back
        // until the answer had already been typed blind.
        write!(w, "{args}").and_then(|()| w.flush())
    };
    match written {
        Ok(()) => Ok(Wrote::Yes),
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(Wrote::ReaderGone),
        Err(e) => Err(e),
    }
}

/// [`write_chunk_result`] with the panic contract the emitters want: a write
/// error that is not `EPIPE` — a full disk behind a redirect — panics exactly
/// as `println!` did, because for them that one IS this run failing; `sink`
/// names the stream it was printing to.
///
/// Split out from [`emit`] so the `EPIPE` arm can be driven against a closed
/// writer without the exit taking the test process with it.
pub(crate) fn write_chunk<W: Write>(
    w: &mut W,
    args: Arguments<'_>,
    newline: bool,
    sink: &str,
) -> Wrote {
    match write_chunk_result(w, args, newline) {
        Ok(wrote) => wrote,
        Err(e) => panic!("failed printing to {sink}: {e}"),
    }
}

/// [`outln!`] / [`out!`] backend — call the macros, not this.
///
/// A reader that left ends the run at exit 0: it is not this run failing, so a
/// pipeline reports whatever the reader returned. The exit is immediate and
/// runs no destructor, which every stdout writer in the crate is already clear
/// of — the session guards live inside `start::run_for_code`, which prints nothing.
pub(crate) fn emit(args: Arguments<'_>, newline: bool) {
    #[cfg(test)]
    if captured(&args, newline) {
        return;
    }
    if write_chunk(&mut std::io::stdout().lock(), args, newline, "stdout") == Wrote::ReaderGone {
        std::process::exit(0);
    }
}

/// [`errln!`] backend — call the macro, not this.
///
/// A reader that left drops the line and returns. `eprintln!` panicked instead:
/// on the main thread that took `exit_code`'s 1 or 2 down to 101 and swallowed
/// the error message on the way, since the panic printed to the same closed
/// stderr; from a background thread it killed the scheduler or watchdog
/// silently. Neither had anywhere left to report the failure, so there is
/// nothing to trade for the line.
pub(crate) fn emit_err(args: Arguments<'_>) {
    #[cfg(test)]
    if captured_err(&args) {
        return;
    }
    let _ = write_chunk(&mut std::io::stderr().lock(), args, true, "stderr");
}

/// [`err!`] backend: [`emit_err`] with no newline, flushed, for a prompt
/// whose answer comes from stdin while stdout stays the command's output.
#[cfg(target_os = "macos")]
pub(crate) fn emit_err_prompt(args: Arguments<'_>) {
    let _ = write_chunk(&mut std::io::stderr().lock(), args, false, "stderr");
}

/// One line on stdout: `println!` with a reader that is allowed to leave.
macro_rules! outln {
    ($($arg:tt)*) => {
        $crate::out::emit(::std::format_args!($($arg)*), true)
    };
}
pub(crate) use outln;

/// Stdout with no trailing newline, flushed — the prompt form of [`outln!`].
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::out::emit(::std::format_args!($($arg)*), false)
    };
}
pub(crate) use out;

/// One line on stderr: `eprintln!` with a reader that is allowed to leave.
macro_rules! errln {
    ($($arg:tt)*) => {
        $crate::out::emit_err(::std::format_args!($($arg)*))
    };
}
pub(crate) use errln;

/// Stderr with no trailing newline, flushed: the prompt form of [`errln!`].
#[cfg(target_os = "macos")]
macro_rules! err {
    ($($arg:tt)*) => {
        $crate::out::emit_err_prompt(::std::format_args!($($arg)*))
    };
}
#[cfg(target_os = "macos")]
pub(crate) use err;

// ── test-only capture ────────────────────────────────────────────────────────
//
// Diverts the stdout bytes one thread writes into a buffer, so a test can pin
// that a TUI action wrote NOTHING to stdout. The same per-thread shape as
// `logline::LogLines` and for the same reason: under a plain `cargo test` run
// every `tests/inline/*.rs` compiles into one binary whose tests are THREADS
// (nextest gives each its own process; `cargo test` does not), so a
// process-global buffer would hand one test its neighbour's bytes and take
// those bytes off the neighbour's own assertions. The emitting thread is the
// unit a test can name — it is the one the test spawned.

#[cfg(test)]
thread_local! {
    static CAPTURE: std::cell::RefCell<Option<OutCapture>> =
        const { std::cell::RefCell::new(None) };
}

/// A capture buffer. Clone it into the thread whose writes are wanted, keep the
/// original to read them back.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct OutCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl OutCapture {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Take every stdout byte the CALLING thread writes until the returned
    /// guard drops. Panics when one is already installed: the inner guard's
    /// drop would silently retire the outer capture, and a byte vanishing is
    /// the one failure a capture harness cannot report on itself.
    #[must_use]
    pub(crate) fn capture_here(&self) -> OutCaptureGuard {
        CAPTURE.with(|c| {
            let mut c = c.borrow_mut();
            assert!(
                c.is_none(),
                "an out capture is already installed on this thread"
            );
            *c = Some(self.clone());
        });
        OutCaptureGuard(std::marker::PhantomData)
    }

    /// The bytes taken so far, in write order.
    pub(crate) fn snapshot(&self) -> Vec<u8> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// RAII half of [`OutCapture::capture_here`]: restores the thread's real sink
/// on drop, panic included.
///
/// `!Send` by construction. A guard is a claim on the thread that installed it,
/// so one carried to another thread would clear that thread's (empty) slot and
/// leave the installing thread capturing for the rest of the process — which
/// presents as a test somewhere else losing its bytes.
#[cfg(test)]
pub(crate) struct OutCaptureGuard(std::marker::PhantomData<*const ()>);

#[cfg(test)]
impl Drop for OutCaptureGuard {
    fn drop(&mut self) {
        // `try_with`, so a drop running during thread teardown restores nothing
        // instead of panicking the thread it exists to report on.
        let _ = CAPTURE.try_with(|c| c.borrow_mut().take());
    }
}

/// Whether this thread's capture took the write, appending the exact bytes
/// [`emit`] would have printed (the trailing newline included when `newline`).
#[cfg(test)]
fn captured(args: &Arguments<'_>, newline: bool) -> bool {
    CAPTURE
        .try_with(|c| {
            c.borrow()
                .as_ref()
                .map(|buf| {
                    let mut line = args.to_string();
                    if newline {
                        line.push('\n');
                    }
                    buf.0
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .extend_from_slice(line.as_bytes());
                })
                .is_some()
        })
        .unwrap_or(false)
}

// ── test-only stderr capture ────────────────────────────────────────────────
//
// The [`emit_err`] half of the stdout capture above: diverts the stderr bytes
// one thread writes into a buffer, so a test can pin exactly what a foreground
// run printed to stderr. The same per-thread shape as [`OutCapture`] and for
// the same reason — under a plain `cargo test` run every `tests/inline/*.rs`
// compiles into one binary whose tests are THREADS.

#[cfg(test)]
thread_local! {
    static ERR_CAPTURE: std::cell::RefCell<Option<ErrCapture>> =
        const { std::cell::RefCell::new(None) };
}

/// A capture buffer for stderr. Clone it into the thread whose writes are
/// wanted, keep the original to read them back.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct ErrCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl ErrCapture {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A stderr capture writing into `out`'s own buffer, so one snapshot holds
    /// both streams in the order the run wrote them. Separate buffers cannot
    /// say whether a stderr line came before or after a stdout row.
    #[cfg(target_os = "linux")]
    pub(crate) fn interleaved_with(out: &OutCapture) -> Self {
        Self(out.0.clone())
    }

    /// Take every stderr byte the CALLING thread writes until the returned
    /// guard drops. Panics when one is already installed, like
    /// [`OutCapture::capture_here`].
    #[must_use]
    pub(crate) fn capture_here(&self) -> ErrCaptureGuard {
        ERR_CAPTURE.with(|c| {
            let mut c = c.borrow_mut();
            assert!(
                c.is_none(),
                "an err capture is already installed on this thread"
            );
            *c = Some(self.clone());
        });
        ErrCaptureGuard(std::marker::PhantomData)
    }

    /// The bytes taken so far, in write order.
    pub(crate) fn snapshot(&self) -> Vec<u8> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// RAII half of [`ErrCapture::capture_here`]: restores the thread's real sink
/// on drop, panic included. `!Send` by construction, for the same reason as
/// [`OutCaptureGuard`].
#[cfg(test)]
pub(crate) struct ErrCaptureGuard(std::marker::PhantomData<*const ()>);

#[cfg(test)]
impl Drop for ErrCaptureGuard {
    fn drop(&mut self) {
        let _ = ERR_CAPTURE.try_with(|c| c.borrow_mut().take());
    }
}

/// Whether this thread's capture took the line [`emit_err`] would have
/// printed (the trailing newline included).
#[cfg(test)]
fn captured_err(args: &Arguments<'_>) -> bool {
    ERR_CAPTURE
        .try_with(|c| {
            c.borrow()
                .as_ref()
                .map(|buf| {
                    let mut line = args.to_string();
                    line.push('\n');
                    buf.0
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .extend_from_slice(line.as_bytes());
                })
                .is_some()
        })
        .unwrap_or(false)
}

#[cfg(test)]
#[path = "../tests/inline/out.rs"]
mod tests;
