use super::*;

// 2026-05-17T14:20:00 UTC == 1779027600 epoch seconds (same fixture instant
// as the fetch ISO tests).
const BASE_UTC: i64 = 1_779_027_600;

#[test]
fn daemon_mode_prefixes_an_iso_utc_stamp() {
    assert_eq!(
        render(true, BASE_UTC, "clauth daemon: switched to 'b'"),
        "2026-05-17T14:20:00+00:00 clauth daemon: switched to 'b'",
        "daemon.log lines must be self-dating — incident forensics depend on it"
    );
}

#[test]
fn daemon_mode_stays_bare_on_stderr() {
    assert_eq!(
        render(false, BASE_UTC, "clauth: 'a' re-authenticated"),
        "clauth: 'a' re-authenticated",
        "the daemon's redirected stderr keeps the historical bare format"
    );
}

#[test]
fn only_a_non_daemon_line_on_a_terminal_diverts_to_the_log_file() {
    // The 2026-07-14 corruption: a background scheduler thread's stderr line
    // paints over the TUI's alternate screen. Diverting to the log file is the
    // fix, and it must fire in exactly one context.
    assert_eq!(
        route(false, true),
        Sink::LogFile,
        "interactive TUI/CLI on a tty"
    );
    assert_eq!(route(false, false), Sink::Stderr, "piped/redirected stderr");
    assert_eq!(
        route(true, true),
        Sink::Stderr,
        "daemon in a foreground console"
    );
    assert_eq!(
        route(true, false),
        Sink::Stderr,
        "daemon under a supervisor"
    );
}

#[test]
fn write_log_line_appends_each_call() {
    let path = std::env::temp_dir().join(format!("clauth-logline-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&path);
    write_log_line(&path, "first");
    write_log_line(&path, "second");
    let body = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        body, "first\nsecond\n",
        "each line appends, none clobbers the prior"
    );
    let _ = std::fs::remove_file(&path);
}

/// What proves the guard's drop restored the real sink is that a SECOND install
/// succeeds at all: `capture_here` asserts the slot is free, so a drop that
/// left the first capture standing panics right there. Emitting into no capture
/// to check the same thing would write the line for real, and on a developer's
/// terminal that sink is the operator's own `~/.clauth/clauth.log`.
#[test]
fn a_capture_takes_the_formatted_line_until_its_guard_drops() {
    let first = LogLines::new();
    let guard = first.capture_here();
    logline!("clauth: {} of {} seen", 2, 3);
    assert_eq!(
        first.snapshot(),
        vec!["clauth: 2 of 3 seen".to_string()],
        "the capture must carry the line as its call site formatted it"
    );
    drop(guard);

    let second = LogLines::new();
    let _guard = second.capture_here();
    logline!("clauth: after the first guard");
    assert_eq!(
        second.snapshot(),
        vec!["clauth: after the first guard".to_string()],
        "the line went somewhere other than the capture standing when it was raised"
    );
}

/// The per-thread scope IS what keeps one test's capture off another's lines
/// under the `cargo test` fallback, where every inline test is a thread of one
/// binary. A process-global sink passes every other test in this file, so the
/// discriminating question is what a thread that installed NOTHING sees.
///
/// Asked of [`captured`] directly rather than by raising a line there: a thread
/// with no capture routes to the real sink, and on a developer's terminal that
/// sink is the operator's own `~/.clauth/clauth.log`. The predicate is the same
/// one [`line`] branches on, and it appends nothing when it answers false.
#[test]
fn a_capture_takes_only_the_lines_raised_on_its_own_thread() {
    let mine = LogLines::new();
    let _guard = mine.capture_here();
    logline!("clauth: raised before the sibling");

    std::thread::scope(|scope| {
        scope.spawn(|| {
            assert!(
                !captured("clauth: probed from the sibling thread"),
                "a thread that installed nothing was handed another thread's capture"
            );
            let theirs = LogLines::new();
            let _guard = theirs.capture_here();
            logline!("clauth: raised on the sibling thread");
            assert_eq!(
                theirs.snapshot(),
                vec!["clauth: raised on the sibling thread".to_string()],
                "the sibling's own capture must take its line"
            );
        });
    });

    logline!("clauth: raised after the sibling");
    assert_eq!(
        mine.snapshot(),
        vec![
            "clauth: raised before the sibling".to_string(),
            "clauth: raised after the sibling".to_string(),
        ],
        "the sibling's install and its drop both reached across threads"
    );
}

/// A guard is a claim on the thread that installed it. Carried to another one
/// its drop clears THAT thread's empty slot and leaves the installer capturing
/// for the rest of the process, which surfaces as some unrelated test quietly
/// losing its lines. The `PhantomData<*const ()>` field is the whole of what
/// prevents the move, and dropping that field breaks nothing else.
#[test]
fn a_capture_guard_cannot_be_carried_to_another_thread() {
    use crate::testutil::{NotSend as _, Probe};

    assert!(Probe::<LogLines>::is_send(), "positive control");
    assert!(
        !Probe::<LogCapture>::is_send(),
        "a guard that can cross threads restores the wrong thread's sink"
    );
}

/// The log lands in `~/.clauth` and carries whatever an event line names
/// (profiles, endpoints, failure bodies), so it rides the same owner-only rule
/// as the rest of the tree.
#[cfg(unix)]
#[test]
fn write_log_line_creates_an_owner_only_file() {
    use std::os::unix::fs::PermissionsExt;

    let path = std::env::temp_dir().join(format!("clauth-logline-perm-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&path);
    write_log_line(&path, "first");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        mode, 0o600,
        "clauth.log mode should be 0o600, got {mode:#o}"
    );
}
