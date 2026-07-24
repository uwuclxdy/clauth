//! In-place log-trim tests (TECH-12 / #39). No launchd, no real ~/.clauth — a
//! plain tempfile stands in for `daemon.log`.

#![allow(clippy::unwrap_used)]

use super::*;

/// Over the cap → trimmed to (at most) `keep`, preserving the TAIL and starting
/// on a line boundary (no leading partial line).
#[test]
fn trims_an_over_cap_log_to_its_tail() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");

    // 40 lines of 100 bytes ≈ 4 KB; cap at 1 KB, keep 512 B.
    let mut body = String::new();
    for i in 0..40 {
        body.push_str(&format!("line {i:03} {}\n", "x".repeat(90)));
    }
    std::fs::write(&log, &body).unwrap();
    let before = std::fs::metadata(&log).unwrap().len();
    assert!(before > 1024, "precondition: log exceeds the cap");

    let trimmed = rotate_log_if_large(&log, 1024, 512).unwrap();
    assert!(trimmed, "an over-cap log must be trimmed");

    let after = std::fs::read_to_string(&log).unwrap();
    assert!(
        after.len() as u64 <= 512,
        "trimmed length {} must be <= keep",
        after.len()
    );
    // The retained region is the tail — the LAST line survives, an early one does not.
    assert!(
        after.contains("line 039"),
        "the newest line is kept: {after:?}"
    );
    assert!(!after.contains("line 000"), "the oldest line is dropped");
    // No leading partial line: the first retained byte begins a whole line.
    assert!(
        after.starts_with("line "),
        "retained region starts on a line boundary: {after:?}"
    );
}

/// Under the cap → untouched, `Ok(false)`.
#[test]
fn leaves_a_small_log_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    std::fs::write(&log, b"tiny\n").unwrap();

    let trimmed = rotate_log_if_large(&log, 1024, 512).unwrap();
    assert!(!trimmed, "a log under the cap must not be rewritten");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "tiny\n");
}

/// Absent file → `Ok(false)`, no error (manual `clauth daemon` → stderr to tty).
#[test]
fn absent_log_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("nonexistent.log");
    assert!(!rotate_log_if_large(&log, 1024, 512).unwrap());
}

/// The boot warning fires for exactly one fd shape: a regular file that is not in
/// append mode (`clauth daemon > daemon.log`), where the in-place trim cannot
/// bound the file. An appending file is the supported setup, and a tty/pipe has
/// no cap to defeat.
#[cfg(unix)]
#[test]
fn only_a_non_append_regular_file_defeats_the_cap() {
    assert!(
        log_cap_defeated(true, false),
        "a truncating `>` redirect: the trim leaves a hole and the log grows unbounded"
    );
    assert!(
        !log_cap_defeated(true, true),
        "launchd's O_APPEND fd resumes at the new EOF — the cap holds"
    );
    assert!(
        !log_cap_defeated(false, false),
        "a tty or pipe has no size cap to defeat"
    );
    assert!(!log_cap_defeated(false, true));
}
