//! CDX-5 P1: HTTP/1.1 primitive parse/compose tests over byte fixtures.

use super::*;

use std::io::BufReader;

fn parse(bytes: &[u8]) -> Result<RequestHead, RequestError> {
    let mut r = BufReader::new(bytes);
    read_request_head(&mut r)
}

#[test]
fn reads_a_well_formed_head() {
    let raw = b"POST /backend-api/codex/responses HTTP/1.1\r\n\
                Host: 127.0.0.1:4517\r\n\
                Content-Length: 12\r\n\
                Authorization: Bearer client-token\r\n\
                Originator: codex_cli_rs\r\n\r\n";
    let head = parse(raw).expect("parse");
    assert_eq!(head.method, "POST");
    assert_eq!(head.target, "/backend-api/codex/responses");
    assert_eq!(head.header("content-length"), Some("12"));
    // Case-insensitive lookup.
    assert_eq!(head.header("AUTHORIZATION"), Some("Bearer client-token"));
    assert_eq!(head.content_length().unwrap(), 12);
}

#[test]
fn tolerates_bare_lf_line_endings() {
    let head = parse(b"GET /backend-api/codex/x HTTP/1.1\nHost: a\n\n").expect("parse");
    assert_eq!(head.target, "/backend-api/codex/x");
    assert_eq!(head.content_length().unwrap(), 0);
}

#[test]
fn chunked_request_is_rejected_with_411() {
    let raw = b"POST /backend-api/codex/x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
    let head = parse(raw).expect("head parses");
    assert_eq!(head.content_length(), Err(RequestError::ChunkedUnsupported));
    assert_eq!(
        RequestError::ChunkedUnsupported.status(),
        "411 Length Required"
    );
}

#[test]
fn oversized_content_length_is_413() {
    let raw = b"POST /backend-api/codex/x HTTP/1.1\r\nContent-Length: 99999999999\r\n\r\n";
    let head = parse(raw).expect("head parses");
    assert_eq!(head.content_length(), Err(RequestError::BodyTooLarge));
}

#[test]
fn invalid_content_length_is_malformed() {
    let raw = b"POST /x HTTP/1.1\r\nContent-Length: not-a-number\r\n\r\n";
    let head = parse(raw).expect("head parses");
    assert!(matches!(
        head.content_length(),
        Err(RequestError::Malformed(_))
    ));
}

#[test]
fn header_with_space_before_colon_is_rejected() {
    // A smuggling vector — the parser must not normalize it.
    let raw = b"POST /x HTTP/1.1\r\nX-Bad : value\r\n\r\n";
    assert!(matches!(parse(raw), Err(RequestError::Malformed(_))));
}

#[test]
fn oversized_head_is_431() {
    let mut raw = b"POST /x HTTP/1.1\r\n".to_vec();
    // One absurdly long header line, no terminator.
    raw.extend_from_slice(b"X-Big: ");
    raw.extend(std::iter::repeat_n(b'a', 70 * 1024));
    let mut r = BufReader::new(&raw[..]);
    assert_eq!(read_request_head(&mut r), Err(RequestError::HeadTooLarge));
}

#[test]
fn connection_closed_mid_head_is_malformed() {
    let raw = b"POST /x HTTP/1.1\r\nHost: a\r\n"; // no blank line
    assert!(matches!(parse(raw), Err(RequestError::Malformed(_))));
}

#[test]
fn strips_identity_framing_and_hop_by_hop_request_headers() {
    for h in [
        "authorization",
        "Authorization",
        "ChatGPT-Account-ID",
        "host",
        "content-length",
        "accept-encoding",
        "connection",
        "transfer-encoding",
        "keep-alive",
        "upgrade",
    ] {
        assert!(is_stripped_request_header(h), "{h} must be stripped");
    }
    for h in [
        "originator",
        "user-agent",
        "session-id",
        "x-codex-turn-state",
    ] {
        assert!(!is_stripped_request_header(h), "{h} must pass through");
    }
}

#[test]
fn compose_upstream_request_keeps_survivors_and_drops_duplicated_identity() {
    let head = RequestHead {
        method: "POST".into(),
        target: "/backend-api/codex/responses".into(),
        headers: vec![
            ("authorization".into(), "Bearer smuggled".into()),
            ("chatgpt-account-id".into(), "acct-smuggled".into()),
            ("originator".into(), "codex_cli_rs".into()),
            ("session-id".into(), "sess-1".into()),
            ("accept-encoding".into(), "gzip, br".into()),
        ],
    };
    let injected = vec![("Authorization", "Bearer real-token".to_string())];
    let bytes = compose_upstream_request(&head, &injected, 0);
    let text = String::from_utf8(bytes).unwrap();
    // Request line verbatim.
    assert!(text.starts_with("POST /backend-api/codex/responses HTTP/1.1\r\n"));
    // Survivors pass through.
    assert!(text.contains("originator: codex_cli_rs\r\n"));
    assert!(text.contains("session-id: sess-1\r\n"));
    // Client-supplied identity + accept-encoding are gone (all casings).
    assert!(!text.to_lowercase().contains("smuggled"));
    assert!(!text.to_lowercase().contains("accept-encoding: gzip"));
    // The injected identity is present exactly once.
    assert_eq!(text.matches("Authorization: Bearer real-token").count(), 1);
}
