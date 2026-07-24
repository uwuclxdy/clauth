//! Shared loopback-OAuth scaffolding (CDX-3 R4): PKCE material, URL
//! percent-codecs, and the localhost callback server — extracted verbatim
//! from `oauth_login.rs` so the codex browser login reuses one hardened
//! implementation instead of a fork. Harness-specific pieces stay with their
//! callers: authorize URLs/scopes/exchange in `oauth_login` (claude) and
//! `codex::login` (codex); this module knows only OAuth mechanics.
//!
//! Parametrized where the harnesses differ: the callback PATH (`/callback`
//! for claude's ephemeral-port flow, `/auth/callback` for codex's registered
//! fixed ports) and the bind strategy ([`BindPort`]).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Base64url without padding (RFC 4648 §5) — the encoding OAuth PKCE mandates.
pub(crate) fn base64url_nopad(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0b11_1111) as usize] as char);
        }
    }
    out
}

/// Percent-encode a query-parameter value (encode everything but RFC 3986
/// unreserved chars) so scope spaces/colons and the redirect URI survive.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Inverse of [`percent_encode`] for the callback query values. `+` → space.
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // Decode from bytes with hex-digit validation. Slicing the &str by
            // byte index (as `from_str_radix(&s[i+1..i+3])` would) panics when a
            // multi-byte UTF-8 char follows a bare '%' — reachable from any local
            // process that hits the loopback port. Validate first, then compute.
            b'%' if i + 3 <= bytes.len()
                && bytes[i + 1].is_ascii_hexdigit()
                && bytes[i + 2].is_ascii_hexdigit() =>
            {
                let hi = (bytes[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
                let lo = (bytes[i + 2] as char).to_digit(16).unwrap_or(0) as u8;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-decoded value of `key` in an `a=1&b=2` query string.
pub(crate) fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// PKCE S256 challenge = base64url(sha256(verifier)).
pub(crate) fn challenge_from_verifier(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64url_nopad(&hasher.finalize())
}

/// `n` CSPRNG bytes, base64url-encoded — used for the verifier and `state`.
pub(crate) fn random_b64url(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("CSPRNG failure: {e}"))?;
    Ok(base64url_nopad(&buf))
}

/// A fresh `(code_verifier, code_challenge)` PKCE pair. 32 random bytes →
/// 43-char verifier, within RFC 7636's 43..128 range.
pub(crate) fn new_pkce() -> Result<(String, String)> {
    let verifier = random_b64url(32)?;
    let challenge = challenge_from_verifier(&verifier);
    Ok((verifier, challenge))
}

/// The request target (path?query) from an HTTP request line: `GET <target> HTTP/1.1`.
pub(crate) fn request_target(request_line: &str) -> Option<&str> {
    let mut parts = request_line.split_whitespace();
    let _method = parts.next()?;
    parts.next()
}

/// Where the callback listener binds.
pub(crate) enum BindPort<'a> {
    /// Any free port (`127.0.0.1:0`) — for providers that accept arbitrary
    /// loopback redirect URIs (claude).
    Ephemeral,
    /// A fixed candidate list, tried in order — for providers whose OAuth
    /// client registers exact redirect URIs (codex: 1455, then 1457).
    Fixed(&'a [u16]),
}

/// Bind the loopback callback listener per `strategy`; returns the listener
/// and the bound port. For [`BindPort::Fixed`], every candidate busy is an
/// actionable error (another login flow — possibly a real `codex login` —
/// may hold the port).
pub(crate) fn bind_loopback(strategy: BindPort<'_>) -> Result<(TcpListener, u16)> {
    match strategy {
        BindPort::Ephemeral => {
            let listener = TcpListener::bind(("127.0.0.1", 0))
                .context("failed to bind the loopback listener for the OAuth callback")?;
            let port = listener.local_addr()?.port();
            Ok((listener, port))
        }
        BindPort::Fixed(ports) => {
            for &port in ports {
                match TcpListener::bind(("127.0.0.1", port)) {
                    Ok(listener) => return Ok((listener, port)),
                    Err(_) => continue,
                }
            }
            anyhow::bail!(
                "every registered callback port ({}) is in use — another login flow \
                 (possibly a running `codex login`) may hold them; finish or stop it and retry",
                ports
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

/// Visual tone of a callback page — picks the card's accent color.
enum Tone {
    Success,
    Warning,
    Danger,
}

impl Tone {
    /// `(dark, light)` accent hex pair (Mocha / Latte semantic colors).
    fn hex(&self) -> (&'static str, &'static str) {
        match self {
            Tone::Success => ("#A6E3A1", "#40A02B"),
            Tone::Warning => ("#F9E2AF", "#DF8E1D"),
            Tone::Danger => ("#F38BA8", "#D20F39"),
        }
    }
}

/// One browser-facing callback page. Copy is always static — OAuth error
/// strings from the query are untrusted input and are never reflected into
/// the HTML (they go to the terminal error only).
struct Page {
    tone: Tone,
    title: &'static str,
    detail: &'static str,
    /// Try `window.close()` after paint. Browsers often refuse to close a tab
    /// a script didn't open, so the copy always covers closing it by hand.
    auto_close: bool,
}

/// Write a small self-contained styled page and close. Dark by default with a
/// light-scheme override; everything is inline, so the page makes no requests.
fn write_response(mut stream: &TcpStream, status: &str, page: Page) {
    let (tone, tone_light) = page.tone.hex();
    let script = if page.auto_close {
        // Let the page paint before trying to close; refusal is expected.
        "<script>setTimeout(function(){window.close()},900)</script>"
    } else {
        ""
    };
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>clauth</title><style>\
         :root{{--bg:#1E1E2E;--raised:#181825;--line:#313244;--text:#CDD6F4;--dim:#A6ADC8;--faint:#7F849C;--tone:{tone}}}\
         @media(prefers-color-scheme:light){{:root{{--bg:#EFF1F5;--raised:#FFFFFF;--line:#CCD0DA;--text:#1E1E2E;--dim:#6C6F85;--faint:#9CA0B0;--tone:{tone_light}}}}}\
         *{{box-sizing:border-box;margin:0;padding:0}}\
         body{{font-family:Onest,ui-sans-serif,system-ui,\"Segoe UI\",sans-serif;background:var(--bg);color:var(--text);min-height:100vh;display:flex;align-items:center;justify-content:center;padding:24px}}\
         main{{background:var(--raised);border:1px solid var(--line);border-left:3px solid var(--tone);border-radius:1px;padding:32px 40px;max-width:440px}}\
         .eyebrow{{font-size:11px;font-weight:450;letter-spacing:.08em;text-transform:uppercase;color:var(--faint);margin-bottom:12px}}\
         h1{{font-size:22px;font-weight:550;letter-spacing:-.01em;margin-bottom:8px}}\
         p{{font-size:14px;line-height:1.55;color:var(--dim)}}\
         </style></head><body><main>\
         <div class=\"eyebrow\">clauth</div><h1>{title}</h1><p>{detail}</p>\
         </main>{script}</body></html>",
        title = page.title,
        detail = page.detail,
    );
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {len}\r\nConnection: close\r\n\r\n{html}",
        len = html.len(),
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// Handle one connection. `Ok(Some(code))` on the real callback path with
/// matching `state`; `Ok(None)` for any unrelated request (keep waiting);
/// `Err` on an OAuth error param or a state mismatch (a security stop).
pub(crate) fn handle_callback(
    stream: TcpStream,
    expected_state: &str,
    callback_path: &str,
) -> Result<Option<String>> {
    // 60 s, not a tight probe-catcher (timeout-sweep 2026-07-18): the caller's
    // 180 s login deadline already bounds the total wait, and this connection
    // may BE the one real browser callback carrying the single-use OAuth code —
    // dropping it on a slow request line (VPN/AV loopback interception, a
    // thrashing machine, WSL2-style forwarded loopback) loses the code with no
    // retry, forcing the whole consent flow over. A stray probe holding a slot
    // for a minute costs nothing; a dropped real callback costs the login.
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
    let mut request_line = String::new();
    // A read failure (half-open browser preconnect, a probe that stalls past the
    // read timeout) must NOT abort the whole login — ignore this connection and
    // keep waiting. Only an OAuth `error` param or a state mismatch is fatal; the
    // overall timeout is enforced by the caller's deadline in `wait_for_code`.
    if BufReader::new(&stream)
        .read_line(&mut request_line)
        .is_err()
    {
        return Ok(None);
    }

    let Some(target) = request_target(&request_line) else {
        write_response(
            &stream,
            "400 Bad Request",
            Page {
                tone: Tone::Danger,
                title: "That request didn't parse",
                detail: "clauth expected an OAuth callback here. clauth is still waiting \
                         for the real callback; you can close this tab.",
                auto_close: false,
            },
        );
        return Ok(None);
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != callback_path {
        write_response(
            &stream,
            "404 Not Found",
            Page {
                tone: Tone::Warning,
                title: "Nothing at this address",
                detail: "The login callback arrives at its own path. \
                         You can close this tab.",
                auto_close: false,
            },
        );
        return Ok(None);
    }
    if let Some(err) = query_param(query, "error") {
        let desc = query_param(query, "error_description").unwrap_or_default();
        // A user-declined consent screen reads differently from a broken flow.
        let page = if err == "access_denied" {
            Page {
                tone: Tone::Warning,
                title: "Login canceled",
                detail: "You declined the authorization request, so no login was \
                         captured. Close this tab; you can retry from clauth any time.",
                auto_close: false,
            }
        } else {
            Page {
                tone: Tone::Danger,
                title: "Login failed",
                detail: "The provider reported an error during authorization. Close this \
                         tab and retry the login from clauth.",
                auto_close: false,
            }
        };
        write_response(&stream, "400 Bad Request", page);
        anyhow::bail!("authorization failed: {err} {desc}");
    }
    let Some(code) = query_param(query, "code") else {
        write_response(
            &stream,
            "400 Bad Request",
            Page {
                tone: Tone::Danger,
                title: "No code in the callback",
                detail: "The redirect arrived without an authorization code. Close \
                         this tab; clauth is still waiting for the real callback.",
                auto_close: false,
            },
        );
        return Ok(None);
    };
    if query_param(query, "state").as_deref() != Some(expected_state) {
        write_response(
            &stream,
            "400 Bad Request",
            Page {
                tone: Tone::Danger,
                title: "Login blocked",
                detail: "This callback didn't match the login clauth started, so it \
                         was rejected for safety. Retry the login from clauth.",
                auto_close: false,
            },
        );
        anyhow::bail!("OAuth state mismatch (possible CSRF); login aborted");
    }
    write_response(
        &stream,
        "200 OK",
        Page {
            tone: Tone::Success,
            title: "You're logged in",
            detail: "clauth captured the login. This tab will try to close itself; \
                     if it sticks around, close it and head back to the terminal.",
            auto_close: true,
        },
    );
    Ok(Some(code))
}

/// Accept loop until the callback arrives or `deadline` passes. Non-callback
/// requests (favicon probes) are answered and ignored.
pub(crate) fn wait_for_code(
    listener: &TcpListener,
    expected_state: &str,
    deadline: Instant,
    callback_path: &str,
) -> Result<String> {
    listener.set_nonblocking(true)?;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for the browser login callback");
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).ok();
                if let Some(code) = handle_callback(stream, expected_state, callback_path)? {
                    return Ok(code);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(anyhow::Error::from(e).context("loopback accept failed")),
        }
    }
}
