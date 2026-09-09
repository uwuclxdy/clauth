//! What a caller sees when it stops reading, driven against the real binary.
//!
//! `src/out.rs` owns both halves, and neither can be exercised from inside a
//! test process: the stdout half ends the run, and the stderr half used to
//! panic. Spawning is the only way to observe the exit code that reaches a
//! shell.
//!
//! Unix only, and not for lack of a Windows story: the child resolves its home
//! through `dirs`, which on Windows reads `FOLDERID_Profile` from the shell API
//! and no environment variable at all, so the run could not be pointed away
//! from the operator's real `~/.clauth`. `HOME_OVERRIDE` is `#[cfg(test)]`
//! state inside the crate, which a spawned binary is not. The contract these
//! tests pin is platform-independent; only the sandbox is not.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;
use std::process::{Child, Command, Stdio};
use tempfile::TempDir;

/// A command that fails, with its home pointed at a sandbox. The `TempDir` goes
/// back to the caller: dropped here it would delete the directory out from
/// under a child that has not finished starting.
fn failing_command(stderr: Stdio) -> (TempDir, Child) {
    let home = tempfile::tempdir().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(["info", "no-such-session-id"])
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("spawn clauth");
    (home, child)
}

/// The regression: `clauth <failing command> 2>&1 | head` reported 101 and
/// printed nothing, because `eprintln!` panicked on the `EPIPE` and took
/// `exit_code`'s mapping with it — while the panic message went to that same
/// closed pipe. A reader that left cannot change the code the run exits with.
#[test]
fn a_failing_command_keeps_its_exit_code_when_stderr_is_closed() {
    let (_home, mut child) = failing_command(Stdio::piped());
    // Closing the read end before the child is anywhere near its error line is
    // what `2>&1 | head -0` does to it.
    drop(child.stderr.take());
    let status = child.wait().expect("wait");
    assert_eq!(
        status.code(),
        Some(1),
        "a closed stderr must not turn a failing run's 1 into a panic's 101"
    );
}

/// The other half of the same contract, and the positive control for the test
/// above: a reader that stayed still gets the whole line. Without this, an
/// `errln!` that emitted nothing at all would pass every assertion in the file.
#[test]
fn a_failing_command_prints_its_error_to_a_reader_that_stayed() {
    let (_home, mut child) = failing_command(Stdio::piped());
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("piped stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(1));
    assert_eq!(
        stderr, "Error: no session found for 'no-such-session-id'\n",
        "the message surface `exit_code` inherited from anyhow, newline included"
    );
}
