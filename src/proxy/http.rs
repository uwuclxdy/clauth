//! Minimal HTTP/1.1 read/write primitives for the CDX-5 loopback proxy
//! (proxy-design.md §1.8). Deliberately tiny — codex is the only client, it
//! sends `Content-Length`-sized JSON request bodies over plain loopback, and
//! the proxy relays a single request per connection (`Connection: close`).
//! Everything here is pure over byte buffers so the parse surface is fully
//! unit-tested; the socket IO lives in `mod.rs`.

use std::io::{BufRead, Read, Write};

/// Header block cap (proxy-design §1.8): robustness, not a security boundary
/// (§1.9 already grants local processes the pool's quota).
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Request body cap — a full resent conversation is single-digit MiB.
const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// A parsed request head: method, request-target, and headers as
/// `(lowercased-name, value)` pairs (order preserved). The body is read
/// separately once `content_length` is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestHead {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
}

/// Why a request was rejected before it could be forwarded — each maps to a
/// specific status the proxy answers (proxy-design §1.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestError {
    /// Malformed request line / headers, or the connection closed mid-head.
    Malformed(String),
    /// Header block exceeded [`MAX_HEAD_BYTES`] → `431`.
    HeadTooLarge,
    /// `Transfer-Encoding: chunked` request → `411 Length Required` (codex
    /// sends sized bodies; the 411 is the explicit contract, never a hang).
    ChunkedUnsupported,
    /// Declared `Content-Length` exceeded [`MAX_BODY_BYTES`] → `413`.
    BodyTooLarge,
}

impl RequestError {
    /// The HTTP status line (code + reason) this error answers with.
    pub(crate) fn status(&self) -> &'static str {
        match self {
            RequestError::Malformed(_) => "400 Bad Request",
            RequestError::HeadTooLarge => "431 Request Header Fields Too Large",
            RequestError::ChunkedUnsupported => "411 Length Required",
            RequestError::BodyTooLarge => "413 Payload Too Large",
        }
    }
}

impl RequestHead {
    /// Case-insensitive lookup of the first header named `name`.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The declared body length: `Content-Length` parsed as a `u64`. `Ok(0)`
    /// when absent (a bodyless request); `Err` on a chunked encoding or a
    /// length past the cap.
    pub(crate) fn content_length(&self) -> Result<u64, RequestError> {
        if let Some(te) = self.header("transfer-encoding")
            && te.to_ascii_lowercase().contains("chunked")
        {
            return Err(RequestError::ChunkedUnsupported);
        }
        let Some(cl) = self.header("content-length") else {
            return Ok(0);
        };
        let len: u64 = cl
            .trim()
            .parse()
            .map_err(|_| RequestError::Malformed(format!("invalid Content-Length: {cl}")))?;
        if len > MAX_BODY_BYTES {
            return Err(RequestError::BodyTooLarge);
        }
        Ok(len)
    }
}

/// Read + parse the request head from `reader` up to (and consuming) the
/// blank line that ends it. Caps the head at [`MAX_HEAD_BYTES`].
pub(crate) fn read_request_head<R: BufRead>(reader: &mut R) -> Result<RequestHead, RequestError> {
    let mut head = Vec::new();
    // Read line by line until an empty line (CRLF CRLF) or the cap.
    loop {
        let mut line = Vec::new();
        let n = read_line_capped(reader, &mut line, MAX_HEAD_BYTES - head.len())?;
        if n == 0 {
            return Err(RequestError::Malformed(
                "connection closed before end of headers".to_string(),
            ));
        }
        // A bare CRLF (or LF) terminates the head.
        let trimmed = strip_eol(&line);
        head.extend_from_slice(&line);
        if trimmed.is_empty() {
            break;
        }
        if head.len() >= MAX_HEAD_BYTES {
            return Err(RequestError::HeadTooLarge);
        }
    }
    parse_head(&head)
}

/// Read one line (through `\n`) into `buf`, but never more than `remaining`
/// bytes — a defense against an endless header line. Returns bytes read.
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    remaining: usize,
) -> Result<usize, RequestError> {
    let mut taken = 0;
    loop {
        let available = reader
            .fill_buf()
            .map_err(|e| RequestError::Malformed(format!("read error: {e}")))?;
        if available.is_empty() {
            return Ok(taken); // EOF
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                buf.extend_from_slice(&available[..=i]);
                taken += i + 1;
                reader.consume(i + 1);
                return Ok(taken);
            }
            None => {
                let take = available.len().min(remaining.saturating_sub(taken));
                buf.extend_from_slice(&available[..take]);
                let consumed = available.len();
                taken += take;
                reader.consume(consumed);
                if taken >= remaining {
                    return Err(RequestError::HeadTooLarge);
                }
            }
        }
    }
}

fn strip_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}

/// Parse a raw head buffer into a [`RequestHead`]. Pure — the unit-test seam.
pub(crate) fn parse_head(head: &[u8]) -> Result<RequestHead, RequestError> {
    let text = std::str::from_utf8(head)
        .map_err(|_| RequestError::Malformed("non-UTF-8 in request head".to_string()))?;
    let mut lines = text.split("\r\n").flat_map(|l| l.split('\n'));
    let request_line = lines
        .next()
        .ok_or_else(|| RequestError::Malformed("empty request".to_string()))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| RequestError::Malformed("no method".to_string()))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| RequestError::Malformed("no request target".to_string()))?
        .to_string();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| RequestError::Malformed(format!("malformed header line: {line}")))?;
        // A header name with whitespace is a smuggling vector (obs-fold /
        // space-before-colon) — reject rather than normalize.
        if name.is_empty() || name.trim() != name {
            return Err(RequestError::Malformed(format!(
                "invalid header name: {name:?}"
            )));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(RequestHead {
        method,
        target,
        headers,
    })
}

/// Read exactly `len` body bytes from `reader`.
pub(crate) fn read_body<R: Read>(reader: &mut R, len: u64) -> std::io::Result<Vec<u8>> {
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    Ok(body)
}

/// Hop-by-hop headers (RFC 7230 §6.1 + the proxy's own set) — never relayed
/// in either direction. Lowercased for a case-insensitive match.
pub(crate) const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The inbound request headers stripped before the proxy injects identity
/// (proxy-design §1.4): the two identity headers (ALL casings/duplicates),
/// framing headers the proxy recomputes, and `accept-encoding` (forced to
/// identity so the relay never owns a decompression story).
pub(crate) fn is_stripped_request_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "authorization" | "chatgpt-account-id" | "host" | "content-length" | "accept-encoding"
    ) || HOP_BY_HOP.contains(&name.as_str())
}

/// Compose the upstream request bytes: request line (target passed through
/// verbatim — the authority is fixed in the URL, §1.4), the surviving inbound
/// headers, the injected identity + framing, then the body. `injected` is the
/// `(name, value)` pairs the proxy owns (Authorization, ChatGPT-Account-ID,
/// Host, Content-Length, Accept-Encoding: identity, Connection: close).
///
/// The production path hands ureq the surviving headers directly (ureq owns
/// the socket), so this whole-buffer composer is the documented wire-shape
/// reference the strip/inject test pins against.
#[cfg(test)]
pub(crate) fn compose_upstream_request(
    head: &RequestHead,
    injected: &[(&str, String)],
    body_len: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + body_len);
    out.extend_from_slice(format!("{} {} HTTP/1.1\r\n", head.method, head.target).as_bytes());
    for (name, value) in &head.headers {
        if is_stripped_request_header(name) {
            continue;
        }
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    for (name, value) in injected {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// Write a small proxy-generated error response (proxy-design §1.8: single
/// request per connection, `Connection: close`).
pub(crate) fn write_error<W: Write>(w: &mut W, status: &str, message: &str) -> std::io::Result<()> {
    let body = format!("clauth proxy: {message}\n");
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    w.write_all(resp.as_bytes())?;
    w.flush()
}

#[cfg(test)]
#[path = "../../tests/inline/proxy_http.rs"]
mod tests;
