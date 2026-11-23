#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

/// The `sessionId` reader accepts only a lowercase Claude Code session id
/// (8-4-4-4-12 hex): an uppercase id, a differently-grouped id, or a
/// wrong-shaped 36-char id never relays.
#[test]
fn is_session_uuid_accepts_only_a_lowercase_uuid() {
    assert!(is_session_uuid("3fde53e3-0a6e-4b11-883e-fca3a218ae52"));
    assert!(!is_session_uuid("3FDE53E3-0A6E-4B11-883E-FCA3A218AE52"));
    assert!(!is_session_uuid("3fde-53e3-0a6e-4b11-883efca3a218ae52"));
    assert!(!is_session_uuid("3fde53e3-0a6e-4b11-883e-fca3a218aegg"));
}
