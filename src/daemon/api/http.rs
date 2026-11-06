//! Just enough HTTP/1.1 to serve the REST API, over any `Read + Write`.
//!
//! Generic over the stream so production drives it with a rustls
//! `StreamOwned<ServerConnection, TcpStream>` and the tests drive it with a
//! plain loopback `TcpStream` — routing, auth, and every parser limit below are
//! therefore testable without a handshake or a certificate.
//!
//! Deliberately not a web framework and deliberately not the one-line parser in
//! `oauth_login.rs` (which discards the method, never reads headers or a body,
//! and has no size cap — fine for a loopback OAuth redirect, not for something
//! listening on a LAN).
//!
//! Persistent connections and pipelining are both supported, which puts the
//! whole burden on message framing being unambiguous. Two rules carry that:
//! `Content-Length` is the ONLY framing accepted (chunked is refused outright,
//! and two `Content-Length` headers that disagree are a hard error), and any
//! framing error closes the connection instead of trying to resynchronize.
//! Together those remove the ambiguity request smuggling is built on: there is
//! never a second reading of where one message ends and the next begins.
//!
//! Pipelined requests are served strictly in order, one at a time, so responses
//! cannot be reordered relative to the requests that produced them.

use std::io::{Read, Write};
use std::time::Instant;

/// Cap on the request line plus headers. A real request here is ~200 bytes.
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// Cap on the body. The API's bodies are one short field each:
/// `{"profile":"<name>"}` and `{"code":"<code>"}`.
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Headers we are willing to parse before calling the request malformed.
const MAX_HEADERS: usize = 32;

/// A parsed request, reduced to what the router and the connection loop need.
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: String,
    /// The `Authorization: Bearer <token>` value, if one was presented.
    pub(crate) bearer: Option<String>,
    /// The `If-None-Match` value, verbatim including its quotes. It is compared
    /// against a tag this server produced, so any normalizing would have to be
    /// done identically on both sides to be worth doing; the status routes —
    /// plain and `?all` — answer conditionally off it.
    pub(crate) if_none_match: Option<String>,
    pub(crate) body: Vec<u8>,
    /// Whether the client is willing to reuse this connection: HTTP/1.1 unless
    /// it said `Connection: close`, HTTP/1.0 only if it asked for keep-alive.
    /// The server may still decide to close anyway.
    pub(crate) keep_alive: bool,
    /// The WebSocket handshake headers, reduced to what the one upgrading
    /// route reads. `requested` is the `Connection: Upgrade` token; the rest
    /// are the eponymous headers, kept verbatim for the accept computation.
    pub(crate) ws: WsHeaders,
    /// The browser half; see [`CrossOrigin`].
    pub(crate) cross_origin: CrossOrigin,
    /// The `Content-Type` header. Only the pairing redemption reads it.
    pub(crate) content_type: SoloHeader,
}

/// The WebSocket half of a [`Request`]. Defaults to "no upgrade asked for", so
/// a request built without one answers as plain HTTP.
#[derive(Default)]
pub(crate) struct WsHeaders {
    pub(crate) requested: bool,
    pub(crate) protocol: Option<String>,
    pub(crate) key: Option<String>,
    pub(crate) version: Option<String>,
}

/// The browser half of a [`Request`]. Defaults to "not from a page".
#[derive(Default)]
pub(crate) struct CrossOrigin {
    pub(crate) origin: SoloHeader,
    /// `Access-Control-Request-Method` was sent. Its value is never read: the
    /// answer to a preflight is the same for every method.
    pub(crate) asks_method: bool,
    /// The connection loop found `origin` present and outside a non-empty
    /// `[serve] app_origins`, so a bearer gets no handler.
    pub(crate) refused: bool,
}

/// A header that means something only when it appears once, as UTF-8. A second
/// copy, or bytes that are not text, can never satisfy a comparison.
#[derive(Default)]
pub(crate) enum SoloHeader {
    #[default]
    Absent,
    One(String),
    Unusable,
}

impl SoloHeader {
    fn see(&mut self, value: &[u8]) {
        *self = match (&*self, std::str::from_utf8(value)) {
            (Self::Absent, Ok(text)) => Self::One(text.to_string()),
            _ => Self::Unusable,
        };
    }

    fn is_present(&self) -> bool {
        !matches!(self, Self::Absent)
    }
}

/// What one answer tells a browser, decided against `[serve] app_origins`. A
/// value here is always a list entry: comparing moves the entry out of the
/// list, so no request byte reaches a response head.
#[derive(Default)]
pub(crate) enum Cors {
    /// The list is empty: no header at all.
    #[default]
    Off,
    /// The list is non-empty and the request's `Origin` is absent or unlisted.
    VaryOnly,
    Allow(String),
    Preflight(String),
}

impl Cors {
    /// Byte-exact against each entry: no case folding, no port rule, no prefix
    /// or suffix test.
    pub(crate) fn judge(listed: &[String], cross_origin: &CrossOrigin) -> Self {
        if listed.is_empty() {
            return Self::Off;
        }
        let SoloHeader::One(origin) = &cross_origin.origin else {
            return Self::VaryOnly;
        };
        listed
            .iter()
            .find(|entry| entry.as_str() == origin.as_str())
            .map_or(Self::VaryOnly, |entry| Self::Allow(entry.clone()))
    }

    /// The `Allow` of an `OPTIONS` asking a method is a preflight. Anything
    /// else stays an ordinary request, judged by the route table.
    pub(crate) fn settle_preflight(self, request: &Request) -> Self {
        match self {
            Self::Allow(entry)
                if request.method == "OPTIONS" && request.cross_origin.asks_method =>
            {
                Self::Preflight(entry)
            }
            other => other,
        }
    }

    pub(crate) fn is_preflight(&self) -> bool {
        matches!(self, Self::Preflight(_))
    }

    /// Whether this verdict stops a bearer: a present `Origin` the list lacks.
    pub(crate) fn refuses(&self, cross_origin: &CrossOrigin) -> bool {
        matches!(self, Self::VaryOnly) && cross_origin.origin.is_present()
    }

    /// The per-request log line's field, in fixed forms: the entry, one word,
    /// or nothing. The raw header never reaches the log.
    pub(crate) fn log_field(&self, cross_origin: &CrossOrigin) -> String {
        match self {
            Self::Allow(entry) | Self::Preflight(entry) => {
                format!(" origin={}", sanitize_for_log(entry))
            }
            Self::Off | Self::VaryOnly if cross_origin.origin.is_present() => {
                " origin=unlisted".to_string()
            }
            Self::Off | Self::VaryOnly => String::new(),
        }
    }

    fn head_lines(&self) -> String {
        match self {
            Self::Off => String::new(),
            Self::VaryOnly => "Vary: Origin\r\n".to_string(),
            Self::Allow(entry) => format!(
                "Access-Control-Allow-Origin: {entry}\r\n\
                 Access-Control-Expose-Headers: ETag\r\n\
                 Vary: Origin\r\n"
            ),
            Self::Preflight(entry) => format!(
                "Access-Control-Allow-Origin: {entry}\r\n\
                 Access-Control-Allow-Methods: {}\r\n\
                 Access-Control-Allow-Headers: Authorization, Content-Type, If-None-Match\r\n\
                 Access-Control-Max-Age: 7200\r\n\
                 Vary: Origin\r\n",
                super::routes::advertised_methods()
            ),
        }
    }
}

impl Request {
    /// Whether the body is declared `application/json`, parameters and case
    /// aside. A page cannot send that type without passing a preflight first.
    pub(crate) fn sends_json(&self) -> bool {
        let SoloHeader::One(value) = &self.content_type else {
            return false;
        };
        let media_type = value.split(';').next().unwrap_or_default();
        trim_ows(media_type.as_bytes()).eq_ignore_ascii_case(b"application/json")
    }

    /// True when `key` appears in the query string as `key`, `key=1`, or
    /// `key=true`. The API has exactly one flag (`?all=1`), so this stays a
    /// scan rather than a parsed map.
    pub(crate) fn flag(&self, key: &str) -> bool {
        self.query.split('&').any(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, "1"));
            k == key && (v == "1" || v.eq_ignore_ascii_case("true"))
        })
    }

    /// The value of `key`, when it was given one. Same deliberately unparsed
    /// scan as [`flag`](Self::flag): the query string has two keys in total.
    pub(crate) fn param(&self, key: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }
}

/// Why a request could not be turned into a [`Request`]. Each maps to a fixed
/// status and a fixed body — nothing from the wire is ever reflected back.
pub(crate) enum RequestError {
    /// Unparseable, truncated, or self-contradictory (e.g. two disagreeing
    /// `Content-Length` headers, the classic request-smuggling setup).
    Malformed,
    HeadTooLarge,
    BodyTooLarge,
    /// Understood but refused: chunked transfer encoding.
    Unsupported,
    /// The connection outlived its budget mid-request. Distinct from `Io` so a
    /// slow sender is told why rather than just dropped.
    Timeout,
    Io(std::io::Error),
}

impl RequestError {
    pub(crate) fn response(&self) -> Response {
        match self {
            Self::Malformed => Response::error(400, "bad_request"),
            Self::HeadTooLarge => Response::error(431, "request_header_fields_too_large"),
            Self::BodyTooLarge => Response::error(413, "payload_too_large"),
            Self::Unsupported => Response::error(400, "chunked_encoding_unsupported"),
            Self::Timeout => Response::error(408, "request_timeout"),
            // Nothing to send: the socket is already broken.
            Self::Io(_) => Response::error(400, "bad_request"),
        }
    }
}

/// Reads successive requests off one connection.
///
/// Owns the read buffer for the connection's whole life, which is what makes
/// pipelining possible: bytes a client sent ahead of the response are the start
/// of the next request, so they are kept rather than rejected. Exactly the
/// bytes one request occupies are drained when it is returned.
///
/// `deadline` bounds the wait in wall-clock time. It is deliberately separate
/// from the socket's own read timeout, which bounds one read: without a
/// wall-clock bound a peer trickling one byte just under the socket timeout
/// could hold a connection slot for days. The connection loop moves this
/// deadline between requests, tighter for the first one than for the idle wait
/// on an established connection.
pub(crate) struct RequestReader<S> {
    stream: S,
    buf: Vec<u8>,
    deadline: Instant,
}

impl<S> RequestReader<S> {
    pub(crate) fn new(stream: S, deadline: Instant) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(1024),
            deadline,
        }
    }

    /// Re-arm the wall-clock bound for the next wait.
    pub(crate) fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }

    /// The underlying stream, for writing responses back.
    pub(crate) fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    pub(crate) fn into_inner(self) -> S {
        self.stream
    }
}

/// A read that returned because the socket's timeout elapsed, rather than
/// because anything went wrong. Unix reports `EAGAIN`, Windows `WSAETIMEDOUT`.
fn is_read_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

impl<S: Read> RequestReader<S> {
    /// Pull more bytes. `started` distinguishes the idle wait between requests
    /// (where a close or an expired budget is the normal, clean end of a
    /// connection) from a read mid-request (where either is a truncation).
    ///
    /// The socket read timeout means two different things depending on where it
    /// lands, and conflating them is what made a kept-alive connection die
    /// after one socket timeout rather than lasting its advertised budget:
    ///
    /// * At a request boundary it means the connection is idle, which is the
    ///   normal state of a kept-alive connection between polls. Wait again, up
    ///   to `deadline`.
    /// * Mid-request it means a peer is trickling bytes to hold a slot. Fail,
    ///   which is what the socket timeout exists for.
    ///
    /// The deadline is therefore checked each time around rather than once, so
    /// a connection that idles out its whole budget still ends promptly instead
    /// of waiting for one more socket timeout.
    fn fill(&mut self, started: bool) -> Result<bool, RequestError> {
        let mut chunk = [0u8; 1024];
        let n = loop {
            if Instant::now() >= self.deadline {
                return if started {
                    Err(RequestError::Timeout)
                } else {
                    Ok(false)
                };
            }
            match self.stream.read(&mut chunk) {
                Ok(n) => break n,
                Err(e) if is_read_timeout(&e) => {
                    if started {
                        return Err(RequestError::Timeout);
                    }
                    // Idle. Re-check the deadline and keep waiting.
                    continue;
                }
                Err(e) => return Err(RequestError::Io(e)),
            }
        };
        if n == 0 {
            return if started {
                Err(RequestError::Malformed)
            } else {
                Ok(false)
            };
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(true)
    }

    /// The next request, or `None` when the peer closed cleanly between
    /// requests (the normal way a kept-alive connection ends).
    ///
    /// The buffer is bounded by one request's worth of head plus body plus the
    /// read that completed it: reads only happen while something is still
    /// missing, so a pipelining client cannot make the server buffer without
    /// limit by sending faster than it is served.
    pub(crate) fn next_request(&mut self) -> Result<Option<Request>, RequestError> {
        // Phase 1: a complete head. Whatever a pipelining client sent ahead is
        // already in `buf`, so this often needs no read at all.
        let head_len = loop {
            let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
            let mut parsed = httparse::Request::new(&mut headers);
            match parsed.parse(&self.buf) {
                Ok(httparse::Status::Complete(n)) => break n,
                Ok(httparse::Status::Partial) => {}
                Err(_) => return Err(RequestError::Malformed),
            }
            // Partial with the cap already buffered means the head itself is
            // over the cap, whatever else is behind it.
            if self.buf.len() >= MAX_HEAD_BYTES {
                return Err(RequestError::HeadTooLarge);
            }
            if !self.fill(!self.buf.is_empty())? {
                return Ok(None);
            }
        };

        // Phase 2: re-parse the now-complete head to pull the fields out.
        // Cheap, and it keeps phase 1 from having to smuggle borrows past a read.
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut parsed = httparse::Request::new(&mut headers);
        if !matches!(
            parsed.parse(&self.buf[..head_len]),
            Ok(httparse::Status::Complete(_))
        ) {
            return Err(RequestError::Malformed);
        }
        let (method, target) = match (parsed.method, parsed.path) {
            // Method names are case-sensitive on the wire, so the verb reaches
            // the router exactly as the client spelled it: normalizing it here
            // would make a lowercase `get` silently answer as `GET`, and the
            // route table is the one place that decides what matches.
            (Some(m), Some(t)) => (m.to_string(), t.to_string()),
            _ => return Err(RequestError::Malformed),
        };
        let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
        let (path, query) = (path.to_string(), query.to_string());
        // httparse reports 1 for HTTP/1.1 and 0 for HTTP/1.0. Anything else is
        // a version this server does not frame for.
        let http_11 = match parsed.version {
            Some(1) => true,
            Some(0) => false,
            _ => return Err(RequestError::Malformed),
        };

        let mut bearer = None;
        let mut if_none_match = None;
        let mut content_length: Option<usize> = None;
        let mut close_requested = false;
        let mut keep_alive_requested = false;
        let mut ws = WsHeaders::default();
        let mut cross_origin = CrossOrigin::default();
        let mut content_type = SoloHeader::default();
        for header in parsed.headers.iter() {
            if header.name.eq_ignore_ascii_case("transfer-encoding") {
                // Refused rather than implemented. With persistent connections
                // this is the load-bearing half of the framing rule: allowing
                // both chunked and Content-Length is what lets two parties
                // disagree about where a message ends.
                return Err(RequestError::Unsupported);
            }
            if header.name.eq_ignore_ascii_case("content-length") {
                let value = parse_content_length(header.value).ok_or(RequestError::Malformed)?;
                // A repeated header agreeing with itself is legal; two different
                // lengths are a smuggling attempt, not a request.
                if content_length.is_some_and(|seen| seen != value) {
                    return Err(RequestError::Malformed);
                }
                content_length = Some(value);
            }
            if header.name.eq_ignore_ascii_case("authorization") {
                bearer = std::str::from_utf8(header.value)
                    .ok()
                    .and_then(strip_bearer)
                    .map(str::to_string);
            }
            if header.name.eq_ignore_ascii_case("if-none-match") {
                // Kept verbatim, quotes included: it is compared against a tag
                // this server produced, so any normalizing would have to be
                // done identically on both sides to be worth doing. A `*` or a
                // list simply fails to match, which serves the full body -- the
                // correct answer, just not the cheapest one.
                if_none_match = std::str::from_utf8(header.value)
                    .ok()
                    .map(|v| v.trim().to_string());
            }
            if header.name.eq_ignore_ascii_case("connection")
                && let Ok(value) = std::str::from_utf8(header.value)
            {
                // A comma-separated token list ("keep-alive, Upgrade").
                for token in value.split(',') {
                    let token = token.trim();
                    if token.eq_ignore_ascii_case("close") {
                        close_requested = true;
                    } else if token.eq_ignore_ascii_case("keep-alive") {
                        keep_alive_requested = true;
                    } else if token.eq_ignore_ascii_case("upgrade") {
                        ws.requested = true;
                    }
                }
            }
            if header.name.eq_ignore_ascii_case("upgrade")
                && ws.protocol.is_none()
                && let Ok(value) = std::str::from_utf8(header.value)
            {
                ws.protocol = Some(value.trim().to_string());
            }
            if header.name.eq_ignore_ascii_case("sec-websocket-key")
                && ws.key.is_none()
                && let Ok(value) = std::str::from_utf8(header.value)
            {
                ws.key = Some(value.trim().to_string());
            }
            if header.name.eq_ignore_ascii_case("sec-websocket-version")
                && ws.version.is_none()
                && let Ok(value) = std::str::from_utf8(header.value)
            {
                ws.version = Some(value.trim().to_string());
            }
            if header.name.eq_ignore_ascii_case("origin") {
                cross_origin.origin.see(header.value);
            }
            if header
                .name
                .eq_ignore_ascii_case("access-control-request-method")
            {
                cross_origin.asks_method = true;
            }
            if header.name.eq_ignore_ascii_case("content-type") {
                content_type.see(header.value);
            }
        }

        // Phase 3: the body, exactly Content-Length bytes. Anything past it is
        // the next pipelined request and stays buffered.
        let want = content_length.unwrap_or(0);
        if want > MAX_BODY_BYTES {
            return Err(RequestError::BodyTooLarge);
        }
        let end = head_len.saturating_add(want);
        while self.buf.len() < end {
            self.fill(true)?;
        }
        let body = self.buf[head_len..end].to_vec();
        self.buf.drain(..end);

        Ok(Some(Request {
            method,
            path,
            query,
            bearer,
            if_none_match,
            body,
            // HTTP/1.1 persists by default; HTTP/1.0 does not unless asked.
            keep_alive: !close_requested && (http_11 || keep_alive_requested),
            ws,
            cross_origin,
            content_type,
        }))
    }
}

impl<S> RequestReader<S> {
    /// The stream plus whatever bytes were read past the last request's end —
    /// the takeover shape for a protocol upgrade: a client may pipeline its
    /// first upgraded frames behind the handshake, and dropping the buffer
    /// would silently lose them.
    pub(crate) fn into_parts(self) -> (S, Vec<u8>) {
        (self.stream, self.buf)
    }
}

/// `Content-Length`, which the grammar defines as `1*DIGIT` and nothing else
/// (RFC 9110 §8.6), with only the OWS the field syntax allows around it.
///
/// Deliberately stricter than `str::parse`, which accepts a leading `+`, and
/// than `str::trim`, which strips every Unicode whitespace character — NBSP
/// included. Either would have this server agree a body length that another
/// parser in the path reads differently, and a disagreement about where a
/// message ends is the whole of request smuggling. Only ASCII SP and HTAB are
/// stripped here, because those are the two characters OWS actually is.
fn parse_content_length(value: &[u8]) -> Option<usize> {
    let digits = trim_ows(value);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // Still fallible: a length too large for `usize` is not a length.
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Strip leading and trailing OWS — ASCII space and horizontal tab, per RFC
/// 9110 — and nothing else.
fn trim_ows(mut value: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = value {
        value = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = value {
        value = rest;
    }
    value
}

/// The token out of an `Authorization` value, or `None` for any other scheme.
/// The scheme is case-insensitive per RFC 7235; the token is not.
fn strip_bearer(value: &str) -> Option<&str> {
    let rest = value.strip_prefix("Bearer ").or_else(|| {
        let (scheme, rest) = value.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then_some(rest)
    })?;
    Some(rest.trim())
}

/// A server-sent-events body: the head is written, then the closure runs on the
/// connection thread, writing frames until the deadline. `FnOnce` because a
/// stream is the rest of the connection, written exactly once.
pub(crate) type StreamWriter =
    Box<dyn FnOnce(&mut dyn Write, Instant) -> std::io::Result<()> + Send>;

/// One response: a status, a body, and whether to challenge for a token.
pub(crate) struct Response {
    pub(crate) status: u16,
    /// The body for every answer but a stream; a stream leaves it empty.
    pub(crate) body: Vec<u8>,
    /// `Some` exactly for a server-sent-events answer. The head is written with
    /// `content_type` and no `Content-Length`, then the closure runs.
    pub(crate) stream: Option<StreamWriter>,
    /// The `Content-Type` head this answer carries: `application/json` for every
    /// plain body, `text/event-stream` for a stream. `into_head` keeps it, so a
    /// `HEAD /events` still names the stream's type.
    pub(crate) content_type: &'static str,
    /// Emit `WWW-Authenticate: Bearer`. Set on 401 so a client knows the scheme
    /// rather than guessing.
    pub(crate) challenge: bool,
    /// Emit `ETag`, so the next request can be conditional.
    pub(crate) etag: Option<String>,
    /// The connection loop stamps this on every answer the router returned, so
    /// a route never decides what a browser may read.
    pub(crate) cors: Cors,
}

/// One error body for every refused or failed answer, so a client generates a
/// single error shape. `reason` rides only the refusals that carry one —
/// today's `error()` answers have no `reason` key at all — so it is skipped
/// when `None`, never serialized as `null`.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ErrorBody {
    pub(crate) ok: bool,
    pub(crate) error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

impl ErrorBody {
    fn new(error: &str, reason: Option<&str>) -> Self {
        Self {
            ok: false,
            error: error.to_string(),
            reason: reason.map(str::to_string),
        }
    }
}

impl Response {
    /// A body that is already serialized JSON — the `status.json` passthrough.
    pub(crate) fn raw_json(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            stream: None,
            content_type: "application/json",
            challenge: false,
            etag: None,
            cors: Cors::Off,
        }
    }

    /// The same, tagged, so a client can ask again conditionally.
    pub(crate) fn raw_json_tagged(status: u16, body: Vec<u8>, etag: String) -> Self {
        Self {
            etag: Some(etag),
            ..Self::raw_json(status, body)
        }
    }

    /// `304 Not Modified`: no body, and the tag repeated so a client that
    /// dropped its copy can re-arm from the response.
    pub(crate) fn not_modified(etag: String) -> Self {
        Self {
            status: 304,
            body: Vec::new(),
            stream: None,
            content_type: "application/json",
            challenge: false,
            etag: Some(etag),
            cors: Cors::Off,
        }
    }

    /// A server-sent-events stream. The head names `content_type` and carries no
    /// `Content-Length`: the body is the rest of the connection, close-delimited.
    pub(crate) fn stream(status: u16, content_type: &'static str, write: StreamWriter) -> Self {
        Self {
            status,
            body: Vec::new(),
            stream: Some(write),
            content_type,
            challenge: false,
            etag: None,
            cors: Cors::Off,
        }
    }

    /// Serialize a typed body to JSON, with the one fallback every answer
    /// shares: a serializer error — unreachable for the plain fields these
    /// bodies hold — still answers the fixed `internal` body rather than
    /// panicking.
    pub(crate) fn serialize<T: serde::Serialize>(status: u16, value: &T) -> Self {
        let body = serde_json::to_vec(value)
            .unwrap_or_else(|_| br#"{"ok":false,"error":"internal"}"#.to_vec());
        Self::raw_json(status, body)
    }

    /// A body already built as a `serde_json::Value`, for the tests that build
    /// one directly.
    #[cfg(test)]
    pub(crate) fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::serialize(status, value)
    }

    /// A fixed error code. `code` is always a literal from this crate, never
    /// anything read off the wire.
    pub(crate) fn error(status: u16, code: &str) -> Self {
        Self::serialize(status, &ErrorBody::new(code, None))
    }

    /// An error carrying clauth's own explanation (a refused switch, say).
    /// `reason` originates in this crate; serde escapes it into the JSON string
    /// either way, so it cannot break out of the body.
    pub(crate) fn refused(status: u16, code: &str, reason: &str) -> Self {
        Self::serialize(status, &ErrorBody::new(code, Some(reason)))
    }

    pub(crate) fn unauthorized() -> Self {
        Self {
            challenge: true,
            ..Self::error(401, "unauthorized")
        }
    }

    /// The HEAD rendering of this answer: every header, no body. RFC 9110 ends
    /// a HEAD response at the blank line regardless of what any header says, so
    /// a body left on ANY answer — the error arms included — desyncs the next
    /// response on a kept-alive connection. The connection loop applies this
    /// to whatever the router produced, one rule for every route and status.
    /// `Content-Length: 0` is deliberate: this server never frames a length
    /// the client must not read. A stream's closure is dropped — a HEAD runs no
    /// stream — and its content type is kept, so `HEAD /events` still names
    /// `text/event-stream`.
    pub(crate) fn into_head(mut self) -> Self {
        self.body.clear();
        self.stream = None;
        self
    }
}

/// What happens to the connection after this response.
///
/// The SERVER's decision, not an echo of the request's: a client's willingness
/// to persist is necessary but not sufficient, since a framing error or an
/// exhausted budget closes regardless.
///
/// `KeepAlive` carries the numbers rather than reading them from a constant, so
/// what the header advertises is necessarily what the connection loop will
/// actually enforce. Advertising a fixed figure is how the header came to
/// promise 120 seconds on a connection that was being dropped after 10.
pub(crate) enum Disposition {
    Close,
    KeepAlive {
        /// Seconds of budget genuinely remaining, not a nominal maximum.
        timeout_secs: u64,
        /// Requests still allowed on this connection.
        max_requests: u32,
    },
}

/// Serialize `resp` onto the stream. Always `no-store` — the body names
/// accounts and their usage.
///
/// Every response states its disposition explicitly rather than relying on the
/// HTTP/1.1 default, so a client is never left inferring whether the connection
/// is still usable. `Content-Length` is always present for a plain body, which
/// is what lets it find the end of this response and the start of the next one.
/// A stream answer carries no length: its body is close-delimited, so the
/// connection is always `close` and the closure runs with the deadline until it
/// returns.
pub(crate) fn write_response<W: Write>(
    w: &mut W,
    resp: Response,
    disposition: &Disposition,
    deadline: Instant,
) -> std::io::Result<()> {
    if let Some(write) = resp.stream {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\n\
             Content-Type: {}\r\n\
             Cache-Control: no-store\r\n\
             Connection: close\r\n",
            resp.status,
            reason_phrase(resp.status),
            resp.content_type,
        );
        if resp.challenge {
            head.push_str("WWW-Authenticate: Bearer\r\n");
        }
        head.push_str(&resp.cors.head_lines());
        head.push_str("\r\n");
        w.write_all(head.as_bytes())?;
        w.flush()?;
        return write(w, deadline);
    }

    let mut head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: {}\r\n",
        resp.status,
        reason_phrase(resp.status),
        resp.content_type,
        resp.body.len(),
        match disposition {
            Disposition::Close => "close",
            Disposition::KeepAlive { .. } => "keep-alive",
        },
    );
    if let Disposition::KeepAlive {
        timeout_secs,
        max_requests,
    } = disposition
    {
        // Advisory, but it saves a client from discovering the idle window and
        // the request budget by being disconnected.
        head.push_str(&format!(
            "Keep-Alive: timeout={timeout_secs}, max={max_requests}\r\n"
        ));
    }
    if resp.challenge {
        head.push_str("WWW-Authenticate: Bearer\r\n");
    }
    if let Some(etag) = &resp.etag {
        head.push_str(&format!("ETag: {etag}\r\n"));
    }
    head.push_str(&resp.cors.head_lines());
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(&resp.body)?;
    w.flush()
}

/// The RFC 6455 magic GUID every accept hash appends to the client's key.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// `Sec-WebSocket-Accept` for a client's `Sec-WebSocket-Key`: base64 of
/// SHA-1 over the key text plus the RFC's GUID. `None` when the key is not the
/// 16 bytes the RFC says a client sends — a key that cannot be the real thing
/// gets the handshake refused rather than hashed anyway.
pub(crate) fn accept_key(client_key: &str) -> Option<String> {
    use base64::Engine as _;
    use sha1::{Digest, Sha1};

    let engine = base64::engine::general_purpose::STANDARD;
    let raw = engine.decode(client_key.trim()).ok()?;
    if raw.len() != 16 {
        return None;
    }
    let mut hasher = Sha1::new();
    hasher.update(client_key.trim().as_bytes());
    hasher.update(WS_GUID.as_bytes());
    Some(engine.encode(hasher.finalize()))
}

/// The `101 Switching Protocols` head that opens an upgraded connection. No
/// `Content-Length` — the rest of the connection is the upgraded protocol, not
/// a body this server could frame.
pub(crate) fn write_upgrade_head<W: Write>(w: &mut W, accept: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\
         \r\n"
    );
    w.write_all(head.as_bytes())?;
    w.flush()
}

/// Flatten control characters in a string taken off the wire. `logline!` is
/// line-oriented and strips nothing, so an embedded newline — in a request
/// path, or in an error carrying an upstream message — would forge a log entry.
pub(crate) fn flatten_control_chars(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// What one sanitized string may contribute to a `daemon.log` line, in chars.
/// A request's method and path are wire bytes bounded only by the 8 KiB header
/// limit (httparse checks token bytes, not length), and the 401 line is written
/// for peers that never presented a credential, so an unbounded carry would let
/// any unauthenticated peer fill the log with chosen bytes.
const LOG_TEXT_LIMIT: usize = 128;

/// Make a string safe for one `daemon.log` line: [`flatten_control_chars`]
/// plus a length bound, with `...` marking a cut that is taken by chars and so
/// never splits a character.
pub(crate) fn sanitize_for_log(s: &str) -> String {
    let flattened = flatten_control_chars(s);
    let mut cleaned: String = flattened.chars().take(LOG_TEXT_LIMIT).collect();
    if flattened.chars().nth(LOG_TEXT_LIMIT).is_some() {
        cleaned.push_str("...");
    }
    cleaned
}

/// One request's `METHOD /path` as it may appear in a `daemon.log` line: the
/// whole summary through [`sanitize_for_log`], because the method is wire
/// bytes too, so bounding the path alone still leaves the flood open.
pub(crate) fn request_summary(method: &str, path: &str) -> String {
    sanitize_for_log(&format!("{method} {path}"))
}

/// Reason phrases for the statuses this server actually emits. Clients key on
/// the numeric status; this is for the human reading a `curl -v`.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_http.rs"]
mod tests;
