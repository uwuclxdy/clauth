//! Inline tests for the Alibaba console login — the console URL's literal `?`,
//! and the callback field lookup across snake/camel spellings and query/body
//! carriers. The loopback listener itself is exercised only through these pure
//! pieces; nothing here opens a socket.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn req(method: &str, query: &str, body: &str) -> Request {
    Request {
        method: method.to_string(),
        query: query.to_string(),
        body: body.to_string(),
        json: None,
    }
}

/// The REAL callback body, captured 2026-08-11 on the international console: a
/// JSON object, snake_case keys only, no camelCase anywhere — and no `state`,
/// which rides the query instead. Verbatim apart from the secret values.
const REAL_BODY: &str = concat!(
    r#"{"access_token":"3f2b1c4d-5e6f-4708-9a1b-2c3d4e5f6071","#,
    r#""api_key":"sk-ws-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567","#,
    r#""base_url":"https://ws-70tgsn2muluq65sn.ap-southeast-1.maas.aliyuncs.com","#,
    r#""console_site":"international","console_region":"ap-southeast-1","#,
    r#""console_switch_agent":"1328727","workspace_id":"ws-70tgsn2muluq65sn"}"#,
);

const REAL_STATE: &str = "0ab3c0e641f70957c75c9c7fee19b816";

/// The captured request, byte for byte apart from the computed length.
fn real_callback() -> Request {
    let raw = format!(
        "POST /?state={REAL_STATE} HTTP/1.1\r\nhost: 127.0.0.1:51234\r\n\
         content-type: application/json\r\ncontent-length: {}\r\n\r\n{REAL_BODY}",
        REAL_BODY.len(),
    );
    read_request(&mut std::io::BufReader::new(raw.as_bytes())).expect("the real callback parses")
}

#[test]
fn the_notice_param_keeps_its_literal_question_mark() {
    // `&state=` there yields a callback with no state at all, which the handler
    // then rejects — the flow hangs until it times out.
    let url = console_login_url(ConsoleSite::International, 51234, "deadbeef");
    assert_eq!(
        url,
        "https://modelstudio.console.alibabacloud.com/console-login\
         ?notice=127.0.0.1:51234?state=deadbeef&needapikey=true"
    );
}

#[test]
fn each_site_opens_its_own_console_front() {
    assert!(
        console_login_url(ConsoleSite::Domestic, 1, "s")
            .starts_with("https://bailian.console.aliyun.com/console-login")
    );
}

#[test]
fn a_callback_field_is_read_in_either_spelling_from_either_carrier() {
    let from_query = req("GET", "access_token=q1&state=s", "");
    assert_eq!(
        field(&from_query, "access_token", "accessToken").as_deref(),
        Some("q1")
    );
    let camel_body = req("POST", "", "accessToken=b1&state=s");
    assert_eq!(
        field(&camel_body, "access_token", "accessToken").as_deref(),
        Some("b1")
    );
}

#[test]
fn a_blank_field_reads_as_absent() {
    // A console that sends `api_key=` must not blank the profile's stored key.
    let blank = req("POST", "", "api_key=&state=s");
    assert_eq!(field(&blank, "api_key", "apiKey"), None);
}

#[test]
fn a_percent_encoded_value_is_decoded_exactly_once() {
    // Decoding twice would turn a token's literal `%20` into a space.
    let encoded = req("GET", "access_token=a%2520b", "");
    assert_eq!(
        field(&encoded, "access_token", "accessToken").as_deref(),
        Some("a%20b")
    );
}

#[test]
fn a_request_body_is_read_past_the_headers_by_content_length() {
    let body = "accessToken=tok&state=s";
    let raw = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let parsed = read_request(&mut std::io::BufReader::new(raw.as_bytes())).expect("parses");
    assert_eq!(parsed.method, "POST");
    assert_eq!(
        field(&parsed, "access_token", "accessToken").as_deref(),
        Some("tok")
    );
    assert_eq!(field(&parsed, "state", "state").as_deref(), Some("s"));
}

#[test]
fn a_lowercase_method_still_matches_the_preflight_check() {
    let parsed = read_request(&mut std::io::BufReader::new(
        &b"options / HTTP/1.1\r\n\r\n"[..],
    ))
    .unwrap();
    assert_eq!(parsed.method, "OPTIONS");
}

/// The one observed login sends a JSON object body, not the `k=v&k=v` form the
/// source-read described. Routing that body through the form parser finds
/// nothing, so every field reads as absent and the login fails AFTER the user
/// has already authorized in the browser.
#[test]
fn the_real_json_callback_body_is_read() {
    let req = real_callback();
    assert_eq!(req.method, "POST");
    assert_eq!(
        field(&req, "access_token", "accessToken").as_deref(),
        Some("3f2b1c4d-5e6f-4708-9a1b-2c3d4e5f6071"),
        "the token lives in the JSON body",
    );
    assert_eq!(
        field(&req, "console_site", "consoleSite").as_deref(),
        Some("international")
    );
    assert_eq!(
        field(&req, "console_region", "consoleRegion").as_deref(),
        Some("ap-southeast-1")
    );
    // `state` is in the QUERY and absent from the body, so the query fallback
    // has to survive alongside the JSON reading.
    assert_eq!(field(&req, "state", "state").as_deref(), Some(REAL_STATE));
}

/// The captured callback drives the whole classification: the session it yields
/// carries the console's own site/region, and the body's workspace `api_key` /
/// `base_url` reach nothing.
#[test]
fn the_real_callback_yields_a_session_and_nothing_else() {
    let req = real_callback();
    let outcome = match classify(&req, REAL_STATE, (ConsoleSite::Domestic, "cn-beijing")) {
        Callback::Captured(o) => o,
        Callback::Rejected(reason) => panic!("the real callback was rejected: {reason}"),
    };
    assert_eq!(
        outcome.console.token,
        "3f2b1c4d-5e6f-4708-9a1b-2c3d4e5f6071"
    );
    // The callback's own site/region beat what the login opened with.
    assert_eq!(outcome.console.site, ConsoleSite::International);
    assert_eq!(outcome.console.region, "ap-southeast-1");
}

/// A body that declares JSON but isn't falls through to the form reading rather
/// than stranding a user who has already authorized.
#[test]
fn a_broken_json_body_still_falls_back_to_the_form_reading() {
    let body = "access_token=tok";
    let raw = format!(
        "POST /?state=s HTTP/1.1\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n\r\n{body}",
        body.len()
    );
    let req = read_request(&mut std::io::BufReader::new(raw.as_bytes())).expect("parses");
    assert!(req.json.is_none(), "it did not parse as a JSON object");
    assert_eq!(
        field(&req, "access_token", "accessToken").as_deref(),
        Some("tok")
    );
}

/// A reader that hands back at most `chunk` bytes per call — what a body split
/// across TCP segments looks like to `Read`.
struct Trickle<'a> {
    rest: &'a [u8],
    chunk: usize,
}

impl std::io::Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.rest.len().min(buf.len()).min(self.chunk);
        buf[..n].copy_from_slice(&self.rest[..n]);
        self.rest = &self.rest[n..];
        Ok(n)
    }
}

/// One `Read::read` returns whatever arrived, not the whole body. A JSON
/// callback split across segments then parses as truncated, falls through to
/// the form reading, finds nothing, and answers 400 — after the user has
/// already authorized, with another 15 minutes of idle to wait out.
#[test]
fn a_body_split_across_segments_is_read_whole() {
    let raw = format!(
        "POST /?state={REAL_STATE} HTTP/1.1\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n\r\n{REAL_BODY}",
        REAL_BODY.len(),
    );
    let mut reader = std::io::BufReader::new(Trickle {
        rest: raw.as_bytes(),
        chunk: 24,
    });
    let req = read_request(&mut reader).expect("parses");
    assert!(
        req.json.is_some(),
        "the JSON body must survive being delivered in pieces"
    );
    assert_eq!(
        field(&req, "access_token", "accessToken").as_deref(),
        Some("3f2b1c4d-5e6f-4708-9a1b-2c3d4e5f6071")
    );
}
