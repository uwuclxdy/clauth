//! Terminal-event sniffer for the relayed SSE stream (proxy-design §1.8).
//!
//! The codex upstream does NOT reliably close the HTTP response after the
//! turn's final `response.completed` event — it can hold the stream open with
//! keepalives, while codex closes its own side the moment it has parsed the
//! terminal event. A relay that only stops on upstream EOF therefore lingers
//! on such turns until a write-to-a-dead-client fails or the agent backstop
//! timeout fires (the 2026-07-18 incident: one spurious "connection error"
//! per turn, plus thread pile-up toward the connection cap). The relay
//! instead sniffs the byte stream for the terminal event and closes both
//! sides once that event has been relayed IN FULL.
//!
//! Wire shape (captured live 2026-07-18, tee on a real turn): every event is
//! an `event: <name>` line, a `data: {"type":"<name>",...}` line, then a
//! blank line — and the terminal data line is HUGE (the entire response
//! object on one line, hundreds of KB). Two consequences the implementation
//! is built around:
//!
//! 1. A line is CLASSIFIED by its prefix as soon as enough bytes arrive —
//!    waiting for the newline of a several-hundred-KB line was the first
//!    sniffer bug (it never matched real turns).
//! 2. Classification only ARMS the sniffer; it FIRES at the next blank line
//!    (the SSE event terminator), so the terminal event's own bytes are
//!    fully relayed before the close. Firing on the prefix was the second
//!    sniffer bug (it truncated the completed event itself — reintroducing
//!    the exact "stream closed before response.completed" failure this
//!    module exists to prevent).
//!
//! Matching is LINE-ANCHORED: a pattern counts only at the start of an SSE
//! line. Model-generated text that literally contains "response.completed"
//! rides inside a delta payload's JSON string — mid-line, with any newline
//! escaped as `\n` (two bytes) — so it can never start a line and can never
//! false-trigger. A format drift upstream simply means no match: the relay
//! degrades to EOF/backstop behavior, never truncates.

/// `event:` name lines that announce a terminal event. EXACT-line matches
/// (`response.completed` must not swallow a hypothetical longer name).
const TERMINAL_EVENT_LINES: &[&[u8]] = &[
    b"event: response.completed",
    b"event: response.failed",
    b"event: response.incomplete",
    // The Responses API's top-level stream error (rate-limit/server error
    // surfaced AFTER the 200 head) also ends the turn — review finding
    // 2026-07-18: without it an errored turn lingers to the backstop.
    b"event: error",
];

/// `data:` payload prefixes of terminal events. Prefix matches — the closing
/// quote guarantees exact event-name matching, and the rest of the (huge)
/// line never needs to be seen.
const TERMINAL_DATA_PREFIXES: &[&[u8]] = &[
    b"data: {\"type\":\"response.completed\"",
    b"data: {\"type\":\"response.failed\"",
    b"data: {\"type\":\"response.incomplete\"",
    b"data: {\"type\":\"error\"",
    b"data: [DONE]",
];

/// Longest pattern is ~36 bytes: once a line has this many bytes its verdict
/// is decidable without seeing the rest.
const DECIDE_AT: usize = 64;

/// Incremental scanner: feed relay chunks as they pass through; returns (and
/// stays) `true` once a terminal event has been seen AND fully terminated by
/// its blank line. Chunk boundaries are invisible to it.
#[derive(Default)]
pub(crate) struct TerminalSniffer {
    /// The current line's first bytes (only up to [`DECIDE_AT`]).
    line: Vec<u8>,
    /// Current line already classified — its remaining bytes are skipped.
    decided: bool,
    /// A terminal anchor line has been seen; fire at the next blank line.
    armed: bool,
    seen: bool,
}

impl TerminalSniffer {
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> bool {
        if self.seen {
            return true;
        }
        for &byte in chunk {
            if byte == b'\n' {
                if !self.decided {
                    let content = strip_cr(&self.line);
                    if content.is_empty() {
                        // Blank line = end of the current SSE event.
                        if self.armed {
                            self.seen = true;
                            return true;
                        }
                    } else if line_is_terminal(content) {
                        self.armed = true;
                    }
                }
                self.line.clear();
                self.decided = false;
            } else if !self.decided {
                self.line.push(byte);
                if self.line.len() >= DECIDE_AT {
                    if line_is_terminal(&self.line) {
                        self.armed = true;
                    }
                    self.decided = true;
                    self.line.clear();
                }
            }
        }
        false
    }
}

fn strip_cr(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

fn line_is_terminal(line: &[u8]) -> bool {
    TERMINAL_EVENT_LINES.contains(&line)
        || TERMINAL_DATA_PREFIXES.iter().any(|p| line.starts_with(p))
}

#[cfg(test)]
#[path = "../../tests/inline/proxy_sse.rs"]
mod tests;
