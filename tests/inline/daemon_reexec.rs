//! Re-exec decision (`ReexecWatch`): a daemon re-execs itself only once two
//! consecutive checks read its binary as replaced, never on a removed one.

use super::ReexecWatch;
use crate::plugin_host::ExeChange;

/// One replaced read is not enough: an install that writes the file in place
/// rather than renaming a finished one over it can still be mid-write at the
/// first read, so the exec waits for a second consecutive read one tick later.
#[cfg(unix)]
#[test]
fn a_replaced_binary_is_due_on_the_second_consecutive_check() {
    let mut watch = ReexecWatch::default();
    assert!(
        !watch.observe(Some(ExeChange::Replaced)),
        "the first replaced read only arms the watch"
    );
    assert!(
        watch.observe(Some(ExeChange::Replaced)),
        "the second consecutive replaced read is due"
    );
}

/// A check reading the binary unchanged between two replaced reads restarts
/// the count.
#[cfg(unix)]
#[test]
fn an_unchanged_read_between_two_replaced_reads_restarts_the_count() {
    let mut watch = ReexecWatch::default();
    assert!(!watch.observe(Some(ExeChange::Replaced)));
    assert!(!watch.observe(None));
    assert!(
        !watch.observe(Some(ExeChange::Replaced)),
        "the streak restarted, so this read only arms it again"
    );
    assert!(watch.observe(Some(ExeChange::Replaced)));
}

/// A removed binary never re-execs: there is nothing to exec, and the exec
/// failure would abort the daemon.
#[test]
fn a_removed_binary_is_never_due() {
    let mut watch = ReexecWatch::default();
    for _ in 0..4 {
        assert!(!watch.observe(Some(ExeChange::Removed)));
    }
}

/// A removed read between two replaced reads also restarts the count (a
/// Windows-style replace leaves the path empty for a moment before the new
/// file lands).
#[cfg(unix)]
#[test]
fn a_removed_read_between_two_replaced_reads_restarts_the_count() {
    let mut watch = ReexecWatch::default();
    assert!(!watch.observe(Some(ExeChange::Replaced)));
    assert!(!watch.observe(Some(ExeChange::Removed)));
    assert!(!watch.observe(Some(ExeChange::Replaced)));
    assert!(watch.observe(Some(ExeChange::Replaced)));
}

/// Windows never re-execs itself: it keeps today's toast.
#[cfg(windows)]
#[test]
fn windows_is_never_due() {
    let mut watch = ReexecWatch::default();
    for _ in 0..4 {
        assert!(!watch.observe(Some(ExeChange::Replaced)));
    }
}
