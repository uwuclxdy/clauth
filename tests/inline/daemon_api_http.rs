//! The API's HTTP layer, driven over an in-memory stream.
//!
//! [`RequestReader`] is generic over `Read`, so every parser limit below is
//! exercised without a socket, a handshake, or a certificate. What these pin is
//! the boundary a LAN-facing listener has to hold: no unbounded allocation, no
//! ambiguity about where one request ends, and no reflecting the wire back into
//! a response.
//!
//! With persistent connections that boundary carries more weight than it used
//! to, because a wrong answer about where a message ends no longer just breaks
//! one request: it mis-frames every request behind it on the same connection.
//! The framing tests here and the real-socket ones in `daemon_api_server.rs`
//! are two halves of the same guarantee.

use super::*;

use std::io::Cursor;
use std::time::Duration;

/// A reader over `raw` with a budget long enough that no test hits it.
fn reader(raw: Vec<u8>) -> RequestReader<Cursor<Vec<u8>>> {
    RequestReader::new(Cursor::new(raw), Instant::now() + Duration::from_secs(3600))
}

/// Parse `raw` as one complete request, the way the connection loop would.
fn parse(raw: &str) -> Result<Request, RequestError> {
    parse_bytes(raw.as_bytes().to_vec())
}

fn parse_bytes(raw: Vec<u8>) -> Result<Request, RequestError> {
    match reader(raw).next_request() {
        Ok(Some(req)) => Ok(req),
        // No test in this file feeds an empty stream, so a clean end of stream
        // here means the request was truncated.
        Ok(None) => Err(RequestError::Malformed),
        Err(e) => Err(e),
    }
}

#[test]
fn parses_a_get_with_a_bearer_token() {
    let req =
        parse("GET /api/v1/status HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer abc123\r\n\r\n")
            .unwrap_or_else(|_| panic!("should parse"));

    assert_eq!(req.method, "GET");
    assert_eq!(req.path, "/api/v1/status");
    assert_eq!(req.query, "");
    assert_eq!(req.bearer.as_deref(), Some("abc123"));
    assert!(req.body.is_empty());
}

#[test]
fn splits_the_query_and_lowercases_nothing_but_the_method() {
    let req = parse("get /api/v1/status?all=1 HTTP/1.1\r\nHost: h\r\n\r\n")
        .unwrap_or_else(|_| panic!("should parse"));
    assert_eq!(req.method, "GET", "the method is normalized");
    assert_eq!(req.path, "/api/v1/status", "the path is not");
    assert_eq!(req.query, "all=1");
    assert!(req.flag("all"));
}

#[test]
fn flag_accepts_the_spellings_a_client_might_send() {
    let cases = [
        ("all=1", true),
        ("all", true),
        ("all=true", true),
        ("all=TRUE", true),
        ("x=1&all=1", true),
        ("all=0", false),
        ("all=false", false),
        ("small=1", false),
        ("", false),
    ];
    for (query, want) in cases {
        let raw = format!("GET /api/v1/status?{query} HTTP/1.1\r\nHost: h\r\n\r\n");
        let req = parse(&raw).unwrap_or_else(|_| panic!("should parse {query:?}"));
        assert_eq!(req.flag("all"), want, "query {query:?}");
    }
}

#[test]
fn reads_a_post_body_of_exactly_content_length() {
    let body = r#"{"profile":"kitty"}"#;
    let raw = format!(
        "POST /api/v1/switch HTTP/1.1\r\nHost: h\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let req = parse(&raw).unwrap_or_else(|_| panic!("should parse"));
    assert_eq!(req.method, "POST");
    assert_eq!(String::from_utf8_lossy(&req.body), body);
}

/// The Authorization scheme is case-insensitive (RFC 7235); the token is not.
#[test]
fn bearer_scheme_is_case_insensitive_and_the_token_is_not() {
    for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
        let raw = format!("GET /api/v1/health HTTP/1.1\r\nAuthorization: {scheme} AbC\r\n\r\n");
        let req = parse(&raw).unwrap_or_else(|_| panic!("should parse {scheme}"));
        assert_eq!(req.bearer.as_deref(), Some("AbC"), "scheme {scheme}");
    }
}

#[test]
fn a_non_bearer_scheme_presents_no_token() {
    let req = parse("GET /api/v1/health HTTP/1.1\r\nAuthorization: Basic dXNlcjpwdw==\r\n\r\n")
        .unwrap_or_else(|_| panic!("should parse"));
    assert!(
        req.bearer.is_none(),
        "Basic credentials must not be read as a bearer token"
    );
}

/// A few enormous header lines, never terminated: the byte cap is the only
/// thing that can stop this, so it is what this pins.
#[test]
fn an_oversized_head_is_refused_before_it_is_buffered() {
    let mut raw = String::from("GET /api/v1/status HTTP/1.1\r\n");
    while raw.len() < MAX_HEAD_BYTES + 1024 {
        raw.push_str(&format!("X-Padding: {}\r\n", "a".repeat(2048)));
    }
    assert!(
        matches!(parse(&raw), Err(RequestError::HeadTooLarge)),
        "an unterminated head must stop at the cap, not grow without bound"
    );
}

/// The other half of the same bound: many small headers hit the header-count
/// limit rather than the byte cap. Different error, same refusal.
#[test]
fn too_many_headers_is_refused() {
    let mut raw = String::from("GET /api/v1/status HTTP/1.1\r\n");
    for i in 0..(MAX_HEADERS + 1) {
        raw.push_str(&format!("X-Pad-{i}: a\r\n"));
    }
    raw.push_str("\r\n");
    assert!(matches!(parse(&raw), Err(RequestError::Malformed)));
}

#[test]
fn a_body_over_the_cap_is_refused_without_reading_it() {
    let raw = format!(
        "POST /api/v1/switch HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
        MAX_BODY_BYTES + 1
    );
    assert!(matches!(parse(&raw), Err(RequestError::BodyTooLarge)));
}

/// Chunked is the other half of every request-smuggling pair and no client of
/// this API needs it, so it is refused rather than implemented.
#[test]
fn chunked_transfer_encoding_is_refused() {
    let raw = "POST /api/v1/switch HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
    assert!(matches!(parse(raw), Err(RequestError::Unsupported)));
}

#[test]
fn disagreeing_content_lengths_are_malformed() {
    let raw = "POST /api/v1/switch HTTP/1.1\r\nContent-Length: 3\r\nContent-Length: 5\r\n\r\nabc";
    assert!(
        matches!(parse(raw), Err(RequestError::Malformed)),
        "two different lengths is a smuggling attempt, not a request"
    );
}

#[test]
fn a_repeated_but_agreeing_content_length_is_accepted() {
    let raw = "POST /api/v1/switch HTTP/1.1\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\nabc";
    let req = parse(raw).unwrap_or_else(|_| panic!("should parse"));
    assert_eq!(req.body, b"abc");
}

#[test]
fn a_non_numeric_content_length_is_malformed() {
    let raw = "POST /api/v1/switch HTTP/1.1\r\nContent-Length: seven\r\n\r\n";
    assert!(matches!(parse(raw), Err(RequestError::Malformed)));
}

/// `Content-Length` is `1*DIGIT`, and every near-miss is refused.
///
/// Each of these parses as a length under some other reasonable-looking rule —
/// `str::parse` takes the `+`, `str::trim` eats the NBSP — and a length this
/// server reads differently from a proxy in front of it is where a smuggled
/// request comes from. There is nothing to gain by being lenient: no client
/// sends these.
#[test]
fn a_content_length_that_is_not_bare_digits_is_malformed() {
    for value in [
        "+5",
        "-0",
        // NBSP: whitespace to `str::trim`, not OWS to the field grammar.
        "5\u{a0}",
        "\u{a0}5",
        "0x5",
        "5.0",
        "5 5",
        "",
        "   ",
        // A real number, and too large for `usize`.
        "99999999999999999999999999999999999999999",
    ] {
        let raw = format!("POST /api/v1/switch HTTP/1.1\r\nContent-Length: {value}\r\n\r\n");
        assert!(
            matches!(parse(&raw), Err(RequestError::Malformed)),
            "Content-Length {value:?} must not be read as a length"
        );
    }
}

/// The OWS the grammar does allow is still accepted, so this tightens the value
/// and not the field syntax.
#[test]
fn a_content_length_padded_with_ows_still_parses() {
    for value in ["3", " 3", "3 ", "\t3\t", "  3  "] {
        let raw = format!("POST /api/v1/switch HTTP/1.1\r\nContent-Length: {value}\r\n\r\nabc");
        let Ok(req) = parse(&raw) else {
            panic!("{value:?} should parse");
        };
        assert_eq!(req.body, b"abc", "{value:?}");
    }
}

/// Bytes behind a complete body are the next pipelined request, and the reader
/// keeps them. Exactly the bytes one request occupies are consumed, which is
/// the whole of pipelining on the server side.
#[test]
fn a_pipelined_second_request_is_read_from_the_same_buffer() {
    let raw = "POST /api/v1/switch HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc\
               GET /api/v1/status?all=1 HTTP/1.1\r\nHost: h\r\n\r\n";
    let mut r = reader(raw.as_bytes().to_vec());

    let first = r
        .next_request()
        .unwrap_or_else(|_| panic!("first should parse"))
        .unwrap_or_else(|| panic!("first should exist"));
    assert_eq!(first.method, "POST");
    assert_eq!(first.body, b"abc", "the body stops at Content-Length");

    let second = r
        .next_request()
        .unwrap_or_else(|_| panic!("second should parse"))
        .unwrap_or_else(|| panic!("second should exist"));
    assert_eq!(second.method, "GET");
    assert_eq!(second.path, "/api/v1/status");
    assert!(second.flag("all"));

    assert!(
        matches!(r.next_request(), Ok(None)),
        "a spent buffer at end of stream is a clean close, not an error"
    );
}

/// Three GETs in one write, the shape a pipelining client actually produces.
#[test]
fn a_whole_pipeline_of_requests_is_drained_in_order() {
    let raw: String = ["/api/v1/health", "/api/v1/status", "/api/v1/nope"]
        .iter()
        .map(|p| format!("GET {p} HTTP/1.1\r\nHost: h\r\n\r\n"))
        .collect();
    let mut r = reader(raw.into_bytes());

    for want in ["/api/v1/health", "/api/v1/status", "/api/v1/nope"] {
        let req = r
            .next_request()
            .unwrap_or_else(|_| panic!("{want} should parse"))
            .unwrap_or_else(|| panic!("{want} should exist"));
        assert_eq!(req.path, want, "requests must come back in order");
    }
    assert!(matches!(r.next_request(), Ok(None)));
}

/// An empty stream is a client that connected and closed without asking for
/// anything, not a malformed request.
#[test]
fn a_clean_close_between_requests_ends_the_connection() {
    assert!(matches!(reader(Vec::new()).next_request(), Ok(None)));
}

#[test]
fn eof_before_the_head_completes_is_malformed() {
    assert!(matches!(
        parse("GET /api/v1/status HTTP/1.1\r\nHost: h\r\n"),
        Err(RequestError::Malformed)
    ));
}

#[test]
fn eof_before_the_body_completes_is_malformed() {
    let raw = "POST /api/v1/switch HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc";
    assert!(matches!(parse(raw), Err(RequestError::Malformed)));
}

#[test]
fn garbage_is_malformed_not_a_panic() {
    assert!(matches!(
        parse_bytes(vec![0xff; 64]),
        Err(RequestError::Malformed)
    ));
}

/// Every parse failure has a response, and none of them is a 200.
#[test]
fn every_request_error_maps_to_a_client_error_status() {
    let cases = [
        (RequestError::Malformed, 400),
        (RequestError::HeadTooLarge, 431),
        (RequestError::BodyTooLarge, 413),
        (RequestError::Unsupported, 400),
    ];
    for (err, want) in cases {
        assert_eq!(err.response().status, want);
    }
}

/// HTTP/1.1 persists by default; HTTP/1.0 does not unless it asks. `close`
/// always wins, whichever version sent it.
#[test]
fn keep_alive_is_negotiated_per_the_request_version_and_header() {
    let cases = [
        ("GET /api/v1/health HTTP/1.1\r\nHost: h\r\n\r\n", true),
        (
            "GET /api/v1/health HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n",
            false,
        ),
        (
            "GET /api/v1/health HTTP/1.1\r\nHost: h\r\nConnection: CLOSE\r\n\r\n",
            false,
        ),
        ("GET /api/v1/health HTTP/1.0\r\nHost: h\r\n\r\n", false),
        (
            "GET /api/v1/health HTTP/1.0\r\nHost: h\r\nConnection: keep-alive\r\n\r\n",
            true,
        ),
        // A token list, which is how `Connection` is actually specified.
        (
            "GET /api/v1/health HTTP/1.1\r\nHost: h\r\nConnection: keep-alive, Upgrade\r\n\r\n",
            true,
        ),
        // `close` alongside anything else still closes.
        (
            "GET /api/v1/health HTTP/1.1\r\nHost: h\r\nConnection: Upgrade, close\r\n\r\n",
            false,
        ),
    ];
    for (raw, want) in cases {
        let req = parse(raw).unwrap_or_else(|_| panic!("should parse: {raw:?}"));
        assert_eq!(req.keep_alive, want, "{raw:?}");
    }
}

fn render(resp: &Response, disposition: &Disposition) -> String {
    let mut out = Vec::new();
    write_response(&mut out, resp, disposition).unwrap_or_else(|_| panic!("write"));
    String::from_utf8(out).unwrap_or_else(|_| panic!("utf8"))
}

fn keep_alive(timeout_secs: u64, max_requests: u32) -> Disposition {
    Disposition::KeepAlive {
        timeout_secs,
        max_requests,
    }
}

#[test]
fn a_response_states_its_connection_disposition_and_is_never_cached() {
    let wire = render(
        &Response::json(200, &serde_json::json!({"ok": true})),
        &Disposition::Close,
    );
    assert!(wire.starts_with("HTTP/1.1 200 OK\r\n"), "{wire}");
    assert!(wire.contains("Connection: close\r\n"), "{wire}");
    assert!(
        wire.contains("Cache-Control: no-store\r\n"),
        "the body names accounts and their usage: {wire}"
    );
    assert!(
        wire.contains("Content-Type: application/json\r\n"),
        "{wire}"
    );
    assert!(wire.ends_with(r#"{"ok":true}"#), "{wire}");
}

/// A kept-alive response has to say so, and has to advertise the budget it will
/// actually honor. Advertising a constant is how the header came to promise 120
/// seconds on a connection being dropped after 10.
#[test]
fn a_kept_alive_response_advertises_the_budget_it_was_given() {
    let wire = render(
        &Response::json(200, &serde_json::json!({"ok": true})),
        &keep_alive(93, 97),
    );
    assert!(wire.contains("Connection: keep-alive\r\n"), "{wire}");
    assert!(
        wire.contains("Keep-Alive: timeout=93, max=97\r\n"),
        "the advertised figures must be the ones passed in, not a constant: {wire}"
    );
}

/// Framing the response is what lets a client find the start of the next one on
/// a reused connection, so the length must be exact for every shape.
#[test]
fn content_length_matches_the_body() {
    for (resp, disposition) in [
        (Response::error(404, "not_found"), Disposition::Close),
        (Response::error(404, "not_found"), keep_alive(120, 100)),
        (Response::unauthorized(), Disposition::Close),
        (
            Response::json(200, &serde_json::json!({"ok": true})),
            keep_alive(1, 1),
        ),
    ] {
        let wire = render(&resp, &disposition);
        let body = wire
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_else(|| panic!("body"));
        assert!(
            wire.contains(&format!("Content-Length: {}\r\n", body.len())),
            "{wire}"
        );
    }
}

#[test]
fn only_the_401_carries_a_bearer_challenge() {
    let unauthorized = render(&Response::unauthorized(), &Disposition::Close);
    assert!(unauthorized.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
    assert!(unauthorized.contains("WWW-Authenticate: Bearer\r\n"));
    assert!(
        !unauthorized.contains("token"),
        "a 401 must not hint at what was wrong with the credential"
    );

    let ok = render(
        &Response::json(200, &serde_json::json!({"ok": true})),
        &Disposition::Close,
    );
    assert!(!ok.contains("WWW-Authenticate"));
}

/// The connection budget is wall-clock, not per-read: without it a peer
/// trickling a byte just under the socket timeout holds a slot indefinitely.
///
/// Which end of the budget you hit matters. Between requests it is just the end
/// of the connection. Part-way through one, the client is owed an answer saying
/// why, so it does not read the disconnect as a network fault and retry
/// forever.
#[test]
fn an_expired_budget_is_a_clean_close_when_idle_and_a_timeout_mid_request() {
    let expired = Instant::now() - Duration::from_secs(1);

    // Nothing buffered: we never saw the start of a request, so there is
    // nothing to answer.
    let mut idle = RequestReader::new(Cursor::new(Vec::new()), expired);
    assert!(matches!(idle.next_request(), Ok(None)));

    // Half a request already read, then the budget ran out. Seeded directly
    // because the alternative is racing a real clock.
    let mut started = RequestReader::new(Cursor::new(Vec::new()), expired);
    started
        .buf
        .extend_from_slice(b"GET /api/v1/status HTTP/1.1\r\nHost: h\r\n");
    assert!(matches!(started.next_request(), Err(RequestError::Timeout)));
    assert_eq!(RequestError::Timeout.response().status, 408);
}

/// A stream that times out `stalls` times before serving `data`, the way an
/// idle socket with a read timeout behaves while a client thinks between polls.
struct StallingRead {
    stalls: usize,
    data: Cursor<Vec<u8>>,
    kind: std::io::ErrorKind,
}

impl Read for StallingRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.stalls > 0 {
            self.stalls -= 1;
            return Err(std::io::Error::new(self.kind, "read timed out"));
        }
        self.data.read(buf)
    }
}

/// The regression this whole change exists for.
///
/// A socket read timeout while WAITING for the next request means the
/// connection is idle, which is the normal state of a kept-alive connection
/// between polls. Treating it as a failure is what closed connections after one
/// 10s timeout while the response advertised a 120s budget.
#[test]
fn an_idle_read_timeout_between_requests_keeps_waiting() {
    // Both spellings: Unix reports EAGAIN, Windows WSAETIMEDOUT.
    for kind in [std::io::ErrorKind::WouldBlock, std::io::ErrorKind::TimedOut] {
        let stream = StallingRead {
            stalls: 5,
            data: Cursor::new(b"GET /api/v1/status HTTP/1.1\r\nHost: h\r\n\r\n".to_vec()),
            kind,
        };
        let mut r = RequestReader::new(stream, Instant::now() + Duration::from_secs(3600));
        let req = r
            .next_request()
            .unwrap_or_else(|_| panic!("{kind:?}: idle timeouts must not end the connection"))
            .unwrap_or_else(|| panic!("{kind:?}: the request should have arrived"));
        assert_eq!(req.path, "/api/v1/status", "{kind:?}");
    }
}

/// The other half: the same timeout MID-request is a peer trickling bytes to
/// hold a slot, and stays fatal. Raising the socket timeout instead of
/// distinguishing the two would have given that peer the whole budget per read.
#[test]
fn a_read_timeout_mid_request_still_fails() {
    let stream = StallingRead {
        stalls: 1,
        data: Cursor::new(Vec::new()),
        kind: std::io::ErrorKind::WouldBlock,
    };
    let mut r = RequestReader::new(stream, Instant::now() + Duration::from_secs(3600));
    // Seed a partial head so the reader is mid-request when the stall lands.
    r.buf
        .extend_from_slice(b"GET /api/v1/status HTTP/1.1\r\nHost: h\r\n");
    assert!(matches!(r.next_request(), Err(RequestError::Timeout)));
}

/// Idling out the whole budget ends the connection promptly rather than after
/// one more socket timeout, and does so cleanly: nobody is owed a response.
#[test]
fn an_idle_connection_ends_when_its_budget_runs_out() {
    let stream = StallingRead {
        // Far more stalls than the budget allows, so the deadline is what stops
        // the loop.
        stalls: 10_000,
        data: Cursor::new(Vec::new()),
        kind: std::io::ErrorKind::WouldBlock,
    };
    let mut r = RequestReader::new(stream, Instant::now() + Duration::from_millis(50));
    assert!(
        matches!(r.next_request(), Ok(None)),
        "an idled-out connection is a clean close, not an error"
    );
}

/// `logline!` is line-oriented and strips nothing, so anything from the wire
/// has to lose its control characters before it reaches `daemon.log`.
#[test]
fn log_sanitizer_flattens_control_characters() {
    let forged = "/api/v1/status\r\n2026-01-01 clauth daemon: switched to 'attacker'";
    let cleaned = sanitize_for_log(forged);
    assert!(!cleaned.contains('\n'));
    assert!(!cleaned.contains('\r'));
    assert_eq!(
        cleaned.len(),
        forged.len(),
        "characters are replaced, not dropped"
    );
}

// ── conditional GET ─────────────────────────────────────────────────────────

/// `If-None-Match` is kept verbatim, quotes included. It is compared against a
/// tag this server produced, so normalizing on one side only would make every
/// conditional GET miss and turn the 304 path into dead weight.
#[test]
fn if_none_match_is_carried_through_verbatim() {
    let req = parse("GET /api/v1/mirror HTTP/1.1\r\nHost: h\r\nIf-None-Match: \"abc123\"\r\n\r\n")
        .unwrap_or_else(|_| panic!("should parse"));

    assert_eq!(req.if_none_match.as_deref(), Some("\"abc123\""));
}

/// Header names are case-insensitive on the wire, and a client is free to spell
/// it however it likes.
#[test]
fn if_none_match_is_matched_case_insensitively() {
    let req = parse("GET /api/v1/mirror HTTP/1.1\r\nHost: h\r\nIF-NONE-MATCH: \"x\"\r\n\r\n")
        .unwrap_or_else(|_| panic!("should parse"));

    assert_eq!(req.if_none_match.as_deref(), Some("\"x\""));
}

/// Absent is the common case, and it has to read as "send me the body" rather
/// than as an empty tag that might accidentally match one.
#[test]
fn a_request_without_the_header_carries_no_tag() {
    let req = parse("GET /api/v1/status HTTP/1.1\r\nHost: h\r\n\r\n")
        .unwrap_or_else(|_| panic!("should parse"));

    assert_eq!(req.if_none_match, None);
}

/// A 304 goes out with no body and an explicit zero length, so a client on a
/// persistent connection knows exactly where the next response starts.
#[test]
fn a_not_modified_response_is_bodyless_and_tagged() {
    let mut out = Vec::new();
    write_response(
        &mut out,
        &Response::not_modified("\"abc\"".to_string()),
        &Disposition::KeepAlive {
            timeout_secs: 30,
            max_requests: 9,
        },
    )
    .unwrap_or_else(|_| panic!("should write"));

    let text = String::from_utf8(out).unwrap_or_else(|_| panic!("utf8"));
    assert!(
        text.starts_with("HTTP/1.1 304 Not Modified\r\n"),
        "got {text}"
    );
    assert!(text.contains("Content-Length: 0\r\n"), "got {text}");
    assert!(text.contains("ETag: \"abc\"\r\n"), "got {text}");
    assert!(
        text.ends_with("\r\n\r\n"),
        "no body follows the head: {text:?}"
    );
}

/// Only a tagged response carries the header, so an untagged route does not
/// invite a conditional request it would never honour.
#[test]
fn an_untagged_response_carries_no_etag() {
    let mut out = Vec::new();
    write_response(
        &mut out,
        &Response::error(404, "not_found"),
        &Disposition::Close,
    )
    .unwrap_or_else(|_| panic!("should write"));

    let text = String::from_utf8(out).unwrap_or_else(|_| panic!("utf8"));
    assert!(!text.contains("ETag"), "got {text}");
}
