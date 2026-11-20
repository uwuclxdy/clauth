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
    // fix, and it must fire only where the process's stderr is, or was, a
    // terminal: never for the daemon or a redirected stderr.
    assert_eq!(
        route(false, false, true),
        Sink::LogFile,
        "interactive TUI/CLI on a tty"
    );
    assert_eq!(
        route(false, false, false),
        Sink::Stderr,
        "piped/redirected stderr"
    );
    assert_eq!(
        route(true, false, true),
        Sink::Stderr,
        "daemon in a foreground console"
    );
    assert_eq!(
        route(true, false, false),
        Sink::Stderr,
        "daemon under a supervisor"
    );
}

/// A TUI pins the log file while its stderr is its terminal, so a line raised
/// after that terminal hung up (stderr no longer a terminal) still goes to the
/// file rather than the dead stderr. Not seen here: `tui::run`'s call and
/// `line`'s read of the pin, which no test drives (the pin is process-global).
#[test]
fn a_diverted_process_keeps_the_log_file_after_its_terminal_is_gone() {
    assert_eq!(
        route(false, true, false),
        Sink::LogFile,
        "a TUI whose terminal hung up"
    );
    assert_eq!(
        route(true, true, false),
        Sink::Stderr,
        "stamping still means the daemon's stderr"
    );
}

#[test]
fn to_logfile_calls_the_durable_sink() {
    let rendered = std::cell::RefCell::new(None);
    to_logfile_with(
        format_args!("clauth: census collected {0}", "deadbeef"),
        |line| {
            *rendered.borrow_mut() = Some(line.to_string());
        },
    );

    let rendered = rendered.into_inner().expect("the durable sink was called");
    assert!(
        rendered.ends_with(" clauth: census collected deadbeef"),
        "the durable sink receives the stamped census diagnostic: {rendered}"
    );
}

/// The census diagnostics' call sites: every branch of the Keychain census
/// raises its line through `census_log!`, and that macro's one emission arm is
/// the durable logfile sink — never `line()`, whose route sends an unstamped
/// non-tty process (exactly `clauth mcp`) to captured stderr, the #81
/// reporting defect. The census body is macOS-only code no Linux run compiles,
/// so the pin is a source scan, the same mechanism the `run_delegate` wiring
/// pins use.
#[test]
fn the_census_diagnostics_route_through_the_durable_logfile_sink() {
    let src = include_str!("../../src/keychain.rs");
    let macro_body = src
        .split_once("macro_rules! census_log")
        .expect("the census log macro exists")
        .1
        .split_once('}')
        .expect("the macro arm is closed")
        .0;
    assert!(
        macro_body.contains("crate::logline::to_logfile"),
        "the macro's emission arm is the durable logfile sink: {macro_body}"
    );
    assert!(
        !macro_body.contains("crate::logline::line"),
        "the macro must not route through `line()`, whose route sends a non-tty process to \
         captured stderr"
    );

    let census_body = src
        .split_once("pub(crate) fn census_namespaced_items()")
        .expect("the census is defined")
        .1
        .split_once("fn dump_keychain")
        .expect("the census body ends where the dump helper begins")
        .0;
    assert_eq!(
        census_body.matches("census_log!(").count(),
        6,
        "every census diagnostic goes through the durable-sink macro"
    );
    assert!(
        !census_body.contains("logline!("),
        "no census diagnostic reaches a raw logline call site"
    );
}

/// The keychain sign-out's restore hint names the switch verb in full: a bare
/// `clauth <name>` reads as a flag for a dash-led name and as clauth's own
/// command for a subcommand name. The sign-out body is macOS-only code no
/// Linux run compiles, so the pin is a source scan (the census pin above).
#[test]
fn the_keychain_sign_out_hint_names_the_switch_command_in_full() {
    let src = include_str!("../../src/keychain.rs");
    let hint = "Run `clauth switch <name>` to put one back";
    let count = |text: &str| text.matches(hint).count();
    assert_eq!(
        count(src),
        1,
        "the sign-out hint must sit in the source once, in the switch form"
    );
    // Positive control: the scan sees a synthetic carrier and rejects the bare
    // form, so a scan that reads nothing cannot keep this pin green.
    assert_eq!(count(&format!("a {hint} b")), 1);
    assert_eq!(count("Run `clauth <name>` to put one back"), 0);
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

/// A `StampOverride` forces [`stamped`] for the calling thread and restores the
/// previous override on drop, so a reader that runs inside the daemon is
/// pinnable from a test process whose sticky process-global `STAMP` never
/// flipped. The outer override is `true`, the inner `false`: the outer value
/// differs from the test-process default, so a drop that restores the default
/// instead of the previous value reds.
#[test]
fn a_stamp_override_forces_stamped_until_its_guard_drops() {
    let _daemon = StampOverride::new(true);
    assert!(stamped(), "the override holds its forced value");
    {
        let _cli = StampOverride::new(false);
        assert!(!stamped(), "a nested override forces cli stamping");
    }
    assert!(
        stamped(),
        "dropping the inner guard restores the outer override"
    );
}

/// A guard is a claim on the thread that installed it; the
/// `PhantomData<*const ()>` field is the whole of what prevents the move.
#[test]
fn a_stamp_override_guard_cannot_be_carried_to_another_thread() {
    use crate::testutil::{NotSend as _, Probe};

    assert!(
        !Probe::<StampOverride>::is_send(),
        "a guard that can cross threads restores the wrong thread's stamp"
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
