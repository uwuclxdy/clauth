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
/// Cap on the body. The only body the API accepts is `{"profile":"<name>"}`.
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
    pub(crate) body: Vec<u8>,
    /// Whether the client is willing to reuse this connection: HTTP/1.1 unless
    /// it said `Connection: close`, HTTP/1.0 only if it asked for keep-alive.
    /// The server may still decide to close anyway.
    pub(crate) keep_alive: bool,
}

impl Request {
    /// True when `key` appears in the query string as `key`, `key=1`, or
    /// `key=true`. The API has exactly one flag (`?all=1`), so this stays a
    /// scan rather than a parsed map.
    pub(crate) fn flag(&self, key: &str) -> bool {
        self.query.split('&').any(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, "1"));
            k == key && (v == "1" || v.eq_ignore_ascii_case("true"))
        })
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
            (Some(m), Some(t)) => (m.to_ascii_uppercase(), t.to_string()),
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
        let mut content_length: Option<usize> = None;
        let mut close_requested = false;
        let mut keep_alive_requested = false;
        for header in parsed.headers.iter() {
            if header.name.eq_ignore_ascii_case("transfer-encoding") {
                // Refused rather than implemented. With persistent connections
                // this is the load-bearing half of the framing rule: allowing
                // both chunked and Content-Length is what lets two parties
                // disagree about where a message ends.
                return Err(RequestError::Unsupported);
            }
            if header.name.eq_ignore_ascii_case("content-length") {
                let value = std::str::from_utf8(header.value)
                    .ok()
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .ok_or(RequestError::Malformed)?;
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
                    }
                }
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
            body,
            // HTTP/1.1 persists by default; HTTP/1.0 does not unless asked.
            keep_alive: !close_requested && (http_11 || keep_alive_requested),
        }))
    }
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

/// One response: a status, a JSON body, and whether to challenge for a token.
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
    /// Emit `WWW-Authenticate: Bearer`. Set on 401 so a client knows the scheme
    /// rather than guessing.
    pub(crate) challenge: bool,
}

impl Response {
    /// A body that is already serialized JSON — the `status.json` passthrough.
    pub(crate) fn raw_json(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            challenge: false,
        }
    }

    pub(crate) fn json(status: u16, value: &serde_json::Value) -> Self {
        let body = serde_json::to_vec(value)
            .unwrap_or_else(|_| br#"{"ok":false,"error":"internal"}"#.to_vec());
        Self::raw_json(status, body)
    }

    /// A fixed error code. `code` is always a literal from this crate, never
    /// anything read off the wire.
    pub(crate) fn error(status: u16, code: &str) -> Self {
        Self::json(status, &serde_json::json!({ "ok": false, "error": code }))
    }

    /// An error carrying clauth's own explanation (a refused switch, say).
    /// `reason` originates in this crate; serde escapes it into the JSON string
    /// either way, so it cannot break out of the body.
    pub(crate) fn refused(status: u16, code: &str, reason: &str) -> Self {
        Self::json(
            status,
            &serde_json::json!({ "ok": false, "error": code, "reason": reason }),
        )
    }

    pub(crate) fn unauthorized() -> Self {
        Self {
            challenge: true,
            ..Self::error(401, "unauthorized")
        }
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
/// is still usable. `Content-Length` is always present, which is what lets it
/// find the end of this response and the start of the next one.
pub(crate) fn write_response<W: Write>(
    w: &mut W,
    resp: &Response,
    disposition: &Disposition,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: {}\r\n",
        resp.status,
        reason_phrase(resp.status),
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
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(&resp.body)?;
    w.flush()
}

/// Flatten control characters before a string reaches `daemon.log`. `logline!`
/// is line-oriented and strips nothing, so an embedded newline — in a request
/// path, or in an error carrying an upstream message — would forge a log entry.
pub(crate) fn sanitize_for_log(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Reason phrases for the statuses this server actually emits. Clients key on
/// the numeric status; this is for the human reading a `curl -v`.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_http.rs"]
mod tests;
