//! Interactive browser login for an Alibaba Model Studio **console** session,
//! used by `clauth login <name>` on a profile whose `base_url` is one of the
//! Model Studio endpoints.
//!
//! This is not OAuth. The console hands the token straight to a loopback port:
//! clauth mints a 16-byte hex `state`, binds `127.0.0.1:0`, and opens
//! `<console>/console-login?notice=127.0.0.1:<port>?state=<state>&needapikey=true`
//! — that second `?` is literal, not an `&`.
//!
//! **The observed callback** (international console, 2026-08-11) is
//! `POST /?state=<state>` with `content-type: application/json` and a JSON
//! object body whose every key is snake_case; `state` rides the QUERY and is
//! absent from the body. That is the shape this module is built for. The query
//! and form-encoded-body paths are kept as fallbacks, and every name is still
//! looked up in camelCase too, because exactly ONE login has been observed and
//! `bailian-cli-commands` 1.14.2 (the source read) describes the other shapes.
//!
//! **The `api_key` the callback carries is deliberately thrown away.** It is a
//! WORKSPACE key (`sk-ws-…`) against the workspace endpoint
//! `ws-<id>.<region>.maas.aliyuncs.com`, a different product with different
//! billing from the Token Plan the profile is on (`sk-sp-…` against
//! `token-plan.<region>.maas.aliyuncs.com`). Writing it onto the profile — even
//! into an empty slot — points that account at a pay-as-you-go endpoint its
//! prepaid plan does not cover. `needapikey=true` nonetheless STAYS on the URL:
//! the one login that is known to call back had it, and dropping it is an
//! unverified change to a verified-working request for no gain now that nothing
//! reads the key.
//!
//! **The captured session's clock is not this login's.** It expires 48h after
//! the operator's aliyun browser sign-in, so re-running `clauth login` inherits
//! whatever is left — minutes, sometimes — rather than restarting it. Nothing
//! here may promise a duration; only a fresh console sign-in buys a full window.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::oauth_login::query_param;
use crate::profile::{ConsoleCredential, ConsoleSite};

/// Console front-ends. `/console-login` is served on both.
const CONSOLE_DOMESTIC: &str = "https://bailian.console.aliyun.com";
const CONSOLE_INTERNATIONAL: &str = "https://modelstudio.console.alibabacloud.com";

/// How long to wait with no callback traffic before giving up. Refreshed on
/// every connection, so a slow console round-trip that keeps probing the port
/// doesn't time out mid-flight.
const IDLE_TIMEOUT_SECS: u64 = 15 * 60;

/// Largest callback body clauth will read. The real payload is a handful of
/// fields; the cap keeps a local process from feeding the listener forever.
const MAX_BODY_BYTES: usize = 64 * 1024;

fn console_base(site: ConsoleSite) -> &'static str {
    match site {
        ConsoleSite::Domestic => CONSOLE_DOMESTIC,
        ConsoleSite::International => CONSOLE_INTERNATIONAL,
    }
}

/// A completed console login: the session, and nothing else.
///
/// The callback also carries `api_key`, `base_url`, `console_switch_agent` and
/// `workspace_id`. NONE of them is kept, and this struct having one field is the
/// enforcement — there is no slot for a caller to write one from. The key and
/// the base url both describe the operator's WORKSPACE (`sk-ws-…` against
/// `ws-<id>.<region>.maas.aliyuncs.com`), not the Token Plan the profile runs
/// on, so persisting either would silently retarget the account onto a
/// pay-as-you-go product. Do not "fix" this by adding them back.
#[derive(Debug, Clone)]
pub(crate) struct ConsoleLoginOutcome {
    pub(crate) console: ConsoleCredential,
}

/// 16 CSPRNG bytes as lowercase hex — the `state` the callback must echo.
fn random_state() -> Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("CSPRNG failure: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// The console URL to open. The `notice` value carries the loopback address and
/// then a LITERAL `?` before `state` — that is the shape the console parses, and
/// an `&` there yields a callback with no state at all.
fn console_login_url(site: ConsoleSite, port: u16, state: &str) -> String {
    format!(
        "{base}/console-login?notice=127.0.0.1:{port}?state={state}&needapikey=true",
        base = console_base(site),
    )
}

/// First line + headers + optional body of one request, as far as this flow
/// cares: the method, the query string, and the body in both readings.
struct Request {
    method: String,
    query: String,
    body: String,
    /// The body as a JSON object, when the request declared
    /// `content-type: application/json` and the bytes parsed into one. This is
    /// the shape a real console login sends; [`Request::body`] stays populated
    /// so the form-encoded fallback still works off the same bytes.
    json: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Read one HTTP request. `None` when the stream yields nothing parseable — a
/// half-open preconnect or a stalled probe must not abort the login.
///
/// Generic over the reader rather than taking the socket, so the parse is
/// testable without binding a port.
fn read_request<R: BufRead>(reader: &mut R) -> Option<Request> {
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let target = parts.next()?;
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");

    // Headers, only to learn the body length and how to read it. A body with no
    // length is treated as absent: this flow never needs chunked decoding.
    let mut content_length = 0usize;
    let mut is_json = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("content-type") {
                // `application/json; charset=utf-8` counts, so match the type
                // rather than the whole header value.
                is_json = value.to_ascii_lowercase().starts_with("application/json");
            }
        }
    }

    let mut body = String::new();
    if content_length > 0 {
        // `read_exact`, not one `read`: a body split across TCP segments would
        // otherwise arrive truncated, parse as broken JSON, fall through to the
        // form reading, find nothing and answer 400 — after the operator has
        // already authorized, with another idle timeout to wait out. The buffer
        // is capped first, so this can only ever block on bytes the sender
        // promised, bounded by the socket's own read timeout.
        let mut buf = vec![0u8; content_length.min(MAX_BODY_BYTES)];
        match reader.read_exact(&mut buf) {
            Ok(()) => body = String::from_utf8_lossy(&buf).into_owned(),
            // A sender that promised more than it delivered gets what arrived;
            // the field lookup below simply misses and the request is rejected.
            Err(_) => body.clear(),
        }
    }
    // A declared JSON body that doesn't parse falls through to the form reading
    // rather than failing the request: the fallbacks cost nothing and a wrong
    // guess here would strand a user who has already authorized in the browser.
    let json = is_json
        .then(|| serde_json::from_str::<serde_json::Value>(&body).ok())
        .flatten()
        .and_then(|v| match v {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        });

    Some(Request {
        method,
        query: query.to_string(),
        body,
        json,
    })
}

/// One callback field, in either spelling, from every carrier the console might
/// use: the JSON body first (the observed shape), then the query, then a
/// form-encoded body. `query_param` already percent-decodes, so nothing decodes
/// twice here. Blank values read as absent, so a `"api_key": ""` can't be
/// mistaken for a value.
fn field(req: &Request, snake: &str, camel: &str) -> Option<String> {
    [snake, camel]
        .into_iter()
        .flat_map(|key| {
            [
                json_field(req, key),
                query_param(&req.query, key),
                query_param(&req.body, key),
            ]
        })
        .flatten()
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
}

/// One string field of a JSON callback body. Non-string values are skipped —
/// every field this flow reads is a string, and coercing a number or an object
/// into one would invent a credential.
fn json_field(req: &Request, key: &str) -> Option<String> {
    req.json.as_ref()?.get(key)?.as_str().map(str::to_string)
}

fn write_plain(mut stream: &TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nAccess-Control-Allow-Origin: *\r\n\
         Content-Type: text/plain; charset=utf-8\r\nContent-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        len = body.len(),
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// CORS preflight: the console asks before it posts.
fn write_preflight(mut stream: &TcpStream) {
    let resp = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\n\
                Access-Control-Allow-Methods: GET, POST, PUT, PATCH, OPTIONS\r\n\
                Access-Control-Allow-Headers: *\r\nContent-Length: 0\r\n\
                Connection: close\r\n\r\n";
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// What one non-preflight request turned out to be.
enum Callback {
    Captured(ConsoleLoginOutcome),
    /// Answered and ignored — the listener keeps waiting. The string is the
    /// plain-text reason sent back, never anything the caller supplied.
    Rejected(&'static str),
}

/// Classify one already-read request. Pure, so the real captured callback can be
/// replayed in a test without binding a port — the JSON-body reading is exactly
/// what a socket test would not have caught.
///
/// A mismatched `state` is rejected rather than aborting the login (which is
/// what the OAuth loopback does): any local process can reach this port, and a
/// wrong state proves only that the caller isn't the console — letting it end
/// the flow would hand any local process a one-request cancel.
fn classify(req: &Request, expected_state: &str, opened: (ConsoleSite, &str)) -> Callback {
    if field(req, "state", "state").as_deref() != Some(expected_state) {
        return Callback::Rejected("state mismatch");
    }
    let Some(token) = field(req, "access_token", "accessToken") else {
        return Callback::Rejected("no access_token");
    };
    let (opened_site, opened_region) = opened;
    // An unrecognised site spelling falls back to the console clauth actually
    // opened — a token minted on one front is meaningless on the other, so
    // guessing from an unknown string is worse than trusting what we chose.
    let site = field(req, "console_site", "consoleSite")
        .and_then(|s| ConsoleSite::parse(&s))
        .unwrap_or(opened_site);
    let region =
        field(req, "console_region", "consoleRegion").unwrap_or_else(|| opened_region.to_string());
    // `api_key` / `base_url` / `workspace_id` are present in the body and are
    // read by nothing: they describe the operator's workspace, not this
    // profile's plan (see [`ConsoleLoginOutcome`]).
    Callback::Captured(ConsoleLoginOutcome {
        console: ConsoleCredential {
            token,
            site,
            region,
        },
    })
}

/// Handle one connection. `Some(outcome)` on the real callback; `None` for a
/// preflight, a probe, a mismatched `state`, or a callback with no token — all
/// of which leave the listener waiting.
fn handle(
    stream: &TcpStream,
    expected_state: &str,
    opened: (ConsoleSite, &str),
) -> Option<ConsoleLoginOutcome> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let req = read_request(&mut BufReader::new(stream))?;
    if req.method == "OPTIONS" {
        write_preflight(stream);
        return None;
    }
    match classify(&req, expected_state, opened) {
        Callback::Captured(outcome) => {
            write_plain(stream, "200 OK", "OK");
            Some(outcome)
        }
        Callback::Rejected(reason) => {
            write_plain(stream, "400 Bad Request", reason);
            None
        }
    }
}

/// Run the console login: open the browser, catch the loopback callback, return
/// the captured session. `on_url` fires just before the browser opens so the CLI
/// can print the URL as a paste fallback — opening is best-effort.
///
/// `site` / `region` are what the profile's `base_url` says the plan is
/// administered from; the callback can override both.
pub(crate) fn login_with(
    site: ConsoleSite,
    region: &str,
    on_url: impl Fn(&str),
) -> Result<ConsoleLoginOutcome> {
    let state = random_state()?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("failed to bind the loopback listener for the console callback")?;
    let port = listener.local_addr()?.port();
    let url = console_login_url(site, port, &state);

    on_url(&url);
    let _ = crate::platform::open_url(&url);

    listener.set_nonblocking(true)?;
    let mut deadline = Instant::now() + Duration::from_secs(IDLE_TIMEOUT_SECS);
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for the console login callback ({} min idle)",
                IDLE_TIMEOUT_SECS / 60
            );
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).ok();
                if let Some(outcome) = handle(&stream, &state, (site, region)) {
                    return Ok(outcome);
                }
                // Traffic arrived, so the console is talking to us — restart the
                // idle clock rather than letting a chatty preflight age out.
                deadline = Instant::now() + Duration::from_secs(IDLE_TIMEOUT_SECS);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(anyhow::Error::from(e).context("loopback accept failed")),
        }
    }
}

/// One-glance summary of a captured console session for `clauth login`. Never
/// prints the token — only its length, which is enough to confirm something
/// real landed.
pub(crate) fn login_summary(outcome: &ConsoleLoginOutcome) -> String {
    format!(
        "  console session: {} chars, site {}, region {}\n  \
         api key: untouched (the console returns a workspace key, not this plan's)\n  \
         window: expires 48h after your aliyun browser sign-in, not after this login\n  \
         (this login inherits what is left of it; a full window needs a console re-sign-in)",
        outcome.console.token.len(),
        outcome.console.site.as_str(),
        outcome.console.region,
    )
}

#[cfg(test)]
#[path = "../tests/inline/alibaba_login.rs"]
mod tests;
