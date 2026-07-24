//! Unit tests for the SSE terminal-event sniffer (src/proxy/sse.rs).

use super::TerminalSniffer;

#[test]
fn fires_at_the_blank_line_after_the_completed_event() {
    let mut s = TerminalSniffer::default();
    assert!(!s.feed(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n"));
    // The terminal data line ARMS; the blank line that ends the event FIRES.
    assert!(!s.feed(b"data: {\"type\":\"response.completed\",\"response\":{}}\n"));
    assert!(s.feed(b"\n"));
    // Sticky once seen.
    assert!(s.feed(b"more bytes"));
}

#[test]
fn huge_terminal_data_line_arms_by_prefix_and_fires_only_after_its_blank_line() {
    // The live wire shape (captured 2026-07-18): the terminal data line
    // carries the ENTIRE response object on one line — hundreds of KB. Two
    // pinned behaviors: (1) the prefix arms without waiting for the line's
    // newline (bug #1: a newline-gated sniffer never matched real turns);
    // (2) it must NOT fire until the event's blank line, so the huge line is
    // relayed in full (bug #2: firing on the prefix truncated the completed
    // event itself).
    let mut s = TerminalSniffer::default();
    let mut line = b"data: {\"type\":\"response.completed\",\"response\":{\"output\":\"".to_vec();
    line.extend(std::iter::repeat_n(b'x', 300_000));
    assert!(!s.feed(&line), "prefix arms but must not fire mid-line");
    assert!(
        !s.feed(b"\"}}\n"),
        "line end reached — event not yet terminated"
    );
    assert!(s.feed(b"\n"), "blank line ends the event → fire");
}

#[test]
fn event_name_line_arms_and_the_events_blank_line_fires() {
    // The `event: <name>` line is a second, field-order-proof anchor. It must
    // not fire before the data payload that follows it has been relayed.
    let mut s = TerminalSniffer::default();
    assert!(!s.feed(
        b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\"}\n\n"
    ));
    assert!(!s.feed(b"event: response.completed\n"));
    assert!(!s.feed(b"data: {\"unknown_order\":1,\"type\":\"response.completed\"}\n"));
    assert!(s.feed(b"\n"));
}

#[test]
fn fires_for_failed_incomplete_and_done_forms() {
    for terminal in [
        &b"data: {\"type\":\"response.failed\",\"response\":{}}\n\n"[..],
        &b"data: {\"type\":\"response.incomplete\",\"response\":{}}\n\n"[..],
        &b"data: [DONE]\n\n"[..],
        &b"event: response.failed\ndata: {\"x\":1}\n\n"[..],
        &b"event: response.incomplete\ndata: {\"x\":1}\n\n"[..],
        // The top-level stream error (post-200 rate-limit/server error) —
        // review finding 2026-07-18: it ends the turn like the response.*
        // terminals do.
        &b"event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\"}\n\n"[..],
        &b"data: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\"}\n\n"[..],
    ] {
        let mut s = TerminalSniffer::default();
        assert!(s.feed(terminal), "should terminate on {terminal:?}");
    }
}

#[test]
fn handles_crlf_line_endings() {
    let mut s = TerminalSniffer::default();
    assert!(!s.feed(b"event: response.completed\r\n"));
    assert!(!s.feed(b"data: {\"type\":\"response.completed\",\"response\":{}}\r\n"));
    assert!(
        s.feed(b"\r\n"),
        "a CR-only line is the blank event terminator"
    );
}

#[test]
fn detects_terminal_split_across_chunk_boundaries() {
    let mut s = TerminalSniffer::default();
    assert!(!s.feed(b"data: {\"type\":\"resp"));
    assert!(!s.feed(b"onse.comp"));
    assert!(!s.feed(b"leted\",\"response\":{}}\n"));
    assert!(s.feed(b"\n"));
}

#[test]
fn ignores_completed_text_inside_a_delta_payload() {
    // Model output quoting the literal event name rides mid-line inside a
    // delta's JSON string — never at line start. Must NOT arm.
    let mut s = TerminalSniffer::default();
    let delta =
        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"data: {\\\"type\\\":\\\"response.completed\\\"}\"}\n\n";
    assert!(!s.feed(delta));
    assert!(
        !s.feed(b"\n\n"),
        "still unarmed — later blank lines are inert"
    );
}

#[test]
fn ignores_prefix_sharing_event_names() {
    // Data patterns end in a closing quote (exact name); event-name lines
    // are exact-line matches — a longer name must not arm.
    let mut s = TerminalSniffer::default();
    assert!(!s.feed(b"data: {\"type\":\"response.completed.fake\",\"x\":1}\n\n"));
    assert!(!s.feed(b"data: {\"type\":\"response.completedish\"}\n\n"));
    assert!(!s.feed(b"event: response.completedish\n\n"));
}

#[test]
fn long_non_terminal_line_resets_cleanly_at_its_newline() {
    let mut s = TerminalSniffer::default();
    // A delta line far past the decision point: classified non-terminal
    // in-flight, skipped to its newline without buffering.
    let mut long = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"".to_vec();
    long.extend(std::iter::repeat_n(b'x', 10_000));
    long.extend_from_slice(b"\"}\n\n");
    assert!(!s.feed(&long));
    // The next event is parsed normally again.
    assert!(s.feed(b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n"));
}
