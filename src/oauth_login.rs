//! Interactive OAuth login for a fresh Claude Code account, shared by the
//! `clauth login` CLI and the TUI Setup tab (login / re-login rows). Both
//! observe the flow through [`LoginProgress`] callbacks.
//!
//! One login, two doors. [`begin_login`] mints ONE PKCE pair and ONE `state`,
//! binds the loopback listener, and builds both URLs ([`LoginLinks`]): the
//! loopback one for a browser on this machine and the hosted one
//! ([`MANUAL_REDIRECT_URI`]) for any other device. [`PendingLogin::run`] then
//! accepts the first code through either door — the loopback callback or a
//! pasted `code#state` — and exchanges it against the redirect that door used.
//! The CLI and the TUI both drive exactly that pair; neither has a flow of its
//! own.
//!
//! Ground truth is the installed Claude Code binary (v2.1.199 for the loopback
//! flow, v2.1.260 for the manual one): the Pro/Max **subscription** login
//! authorizes at `claude.com/cai/oauth/authorize` (`CLAUDE_AI_AUTHORIZE_URL`
//! — the `platform.claude.com` host is the Console/API-billing surface and does
//! NOT mint claude.ai credentials), sends `code=true` plus the 6-scope set below.
//! The code is then exchanged at `platform.claude.com/v1/oauth/token` via
//! [`crate::oauth`] with whichever redirect delivered it. The authorize-host
//! risk knob is documented on [`AUTHORIZE_URL`].

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::logline::logline;
use crate::profile::{AccountId, ClaudeCredentials, OAuthToken};
use crate::usage::{PlanInfo, now_ms};

/// Claude Code's authorize endpoint for a Pro/Max **subscription** login
/// (`CLAUDE_AI_AUTHORIZE_URL` in v2.1.199). The `platform.claude.com/oauth/authorize`
/// host is Console/API-billing and does NOT mint claude.ai credentials — if a live
/// login 4xx's or shows an API-key consent screen, that host is the fallback knob.
const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";

/// Claude Code's manual redirect (`MANUAL_REDIRECT_URL` in v2.1.260). Instead of a
/// loopback port, the authorize page lands on this platform.claude.com page,
/// which shows the user a `code#state` string to paste back. Verified in
/// v2.1.260: every `/login` builds both URLs from one PKCE pair and one `state`,
/// prints this one under "Browser didn't open? Visit:", and sends it as the
/// exchange's `redirect_uri` whenever the paste, not the listener, delivered the
/// code. If a live paste-door login 4xx's at authorize time, this constant is the
/// knob: Anthropic moved the platform host once already (console.anthropic.com).
pub(crate) const MANUAL_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";

/// Longest pasted `code#state` accepted, in bytes. A real one is a few hundred;
/// the cap bounds what a piped stdin or a TUI paste can make the process hold.
pub(crate) const MANUAL_CODE_MAX: usize = 4096;

/// Which door delivered the code. The exchange's `redirect_uri` follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginMethod {
    /// The loopback callback from the browser on this host.
    Browser,
    /// A pasted `code#state` from the hosted link.
    Manual,
}

/// The 6-scope union Claude Code requests for an interactive login (verbatim from
/// v2.1.199's `ALL_OAUTH_SCOPES`). `org:create_api_key` is Console-only but rides
/// the claude.ai path harmlessly; drop it first if authorize rejects the scope set.
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// How long to wait for a code through either door — the loopback callback or a
/// pasted `code#state` — before giving up.
const LOGIN_TIMEOUT_SECS: u64 = 600;

/// The WHOLE deadline one accepted callback connection carries for delivering
/// its request line, armed when the listener accepts it and never reset by a
/// byte. Raised from the old serial accept's 10 s read bound to 60 s per
/// cloudy's ruling: a slow request line still yields the code, while a silent
/// or trickling socket ends at the deadline. A per-read timeout alone would let
/// a peer sending one byte just under the bound hold its reader for days, so
/// [`read_callback_head`] re-arms each read to the time REMAINING to this
/// deadline (the `daemon::bridge::REQUEST_DEADLINE` shape).
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The whole-connection bound [`wait_for_code`] arms at accept:
/// [`CALLBACK_READ_TIMEOUT`], or a shorter value a test poses a slow or
/// trickling request line under. The one source, so the listener and its tests
/// read the same number; production never sets the override.
pub(crate) fn callback_read_timeout() -> Duration {
    #[cfg(test)]
    if let Some(t) = *CALLBACK_READ_TIMEOUT_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return t;
    }
    CALLBACK_READ_TIMEOUT
}

/// Test seam shortening the callback connection deadline so a slow or trickling
/// request line is drivable in milliseconds, the `lock::STATE_LOCK_TIMEOUT_OVERRIDE`
/// shape. `None` is the production deadline.
#[cfg(test)]
static CALLBACK_READ_TIMEOUT_OVERRIDE: std::sync::Mutex<Option<Duration>> =
    std::sync::Mutex::new(None);

/// Set or clear the test-only connection-deadline override. `None` restores
/// [`CALLBACK_READ_TIMEOUT`]. `HomeSandbox` also clears it on new and drop.
#[cfg(test)]
pub(crate) fn set_callback_read_timeout_override(timeout: Option<Duration>) {
    *CALLBACK_READ_TIMEOUT_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = timeout;
}

/// Cap on one callback request's head (the request line plus headers). A real
/// one is a few hundred bytes, so 16 KiB is far above it, while a peer cannot
/// make the reader buffer an unbounded line (the `daemon::bridge::MAX_HEAD_BYTES`
/// shape, SB-10).
pub(crate) const MAX_CALLBACK_HEAD_BYTES: usize = 16 * 1024;

/// Live callback reader threads, counted across both callback listeners, one
/// process may hold at once. Each holds a socket for up to
/// [`CALLBACK_READ_TIMEOUT`], so the cap is what stops a local
/// peer opening connections in a loop from exhausting threads and memory (the
/// `daemon::api::MAX_CONNECTIONS` precedent).
const MAX_CALLBACK_READERS: usize = 32;

/// Reader threads currently live across both callback listeners. The slot is
/// released however the reader ends, panic included.
static LIVE_CALLBACK_READERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Whether the cap's refusal has already been logged for this saturation
/// episode; re-armed below half the cap, so a flood logs on its transition
/// rather than once per dropped connection (the `daemon::api` hysteresis).
static CALLBACK_READERS_SATURATED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// A test's cap for the listener whose accept loop runs on THIS thread, plus the
// override's OWN live-reader counter. Not process-wide, unlike the bound
// overrides: `cargo test` runs sibling tests as threads of one process, and a
// process-wide tiny cap would refuse THEIR callback readers (a frozen test red
// inside another test's window). The accept loop calls `spawn_callback_reader`
// on the thread that entered `wait_for_code`, so a thread-local override
// reaches exactly its own listener — and a reader spawned under it counts
// against the override's counter alone, so a sibling test's process-wide
// reader can never fill this cap.
#[cfg(test)]
thread_local! {
    static CALLBACK_READER_CAP_OVERRIDE:
        std::cell::RefCell<Option<(usize, std::sync::Arc<std::sync::atomic::AtomicUsize>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Set or clear the test-only live-reader cap on the CALLING thread, which must
/// be the thread that drives the listener's accept loop. `Some` arms a fresh
/// live-reader counter so the cap starts empty; `None` restores the
/// process-wide counter.
#[cfg(test)]
pub(crate) fn set_callback_reader_cap_override(cap: Option<usize>) {
    CALLBACK_READER_CAP_OVERRIDE.with(|c| {
        *c.borrow_mut() = cap.map(|limit| {
            (
                limit,
                std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
        });
    });
}

/// The live-reader cap and the counter a slot counts against:
/// [`MAX_CALLBACK_READERS`] on the process-wide counter, or — under a test's
/// cap override — the override's own cap and counter. `HomeSandbox` also
/// clears the override.
fn callback_reader_target() -> (LiveReaderCounter, usize) {
    #[cfg(test)]
    if let Some((cap, counter)) = CALLBACK_READER_CAP_OVERRIDE.with(|c| c.borrow().clone()) {
        return (LiveReaderCounter::override_of(counter), cap);
    }
    (LiveReaderCounter::process(), MAX_CALLBACK_READERS)
}

/// Where a live reader slot's count lives: the process-wide
/// [`LIVE_CALLBACK_READERS`] (`None`), or a test cap override's own counter
/// (`Some`), captured at spawn and released by the slot's `Drop`. The override
/// counter is an `Arc` so the reader thread can release it however the reader
/// ends, panic included, without touching the process-wide count a sibling
/// test's readers hold up.
struct LiveReaderCounter(Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>);

impl LiveReaderCounter {
    fn process() -> Self {
        Self(None)
    }

    #[cfg(test)]
    fn override_of(counter: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Self(Some(counter))
    }

    fn fetch_add(&self, n: usize) -> usize {
        match &self.0 {
            None => LIVE_CALLBACK_READERS.fetch_add(n, std::sync::atomic::Ordering::Relaxed),
            Some(counter) => counter.fetch_add(n, std::sync::atomic::Ordering::Relaxed),
        }
    }

    fn fetch_sub(&self, n: usize) -> usize {
        match &self.0 {
            None => LIVE_CALLBACK_READERS.fetch_sub(n, std::sync::atomic::Ordering::Relaxed),
            Some(counter) => counter.fetch_sub(n, std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// One live reader's cap slot. `Drop` releases it however the reader ends.
struct CallbackReaderSlot {
    counter: LiveReaderCounter,
}

impl CallbackReaderSlot {
    fn acquire() -> Option<Self> {
        let (counter, cap) = callback_reader_target();
        let live = counter.fetch_add(1) + 1;
        if live > cap {
            counter.fetch_sub(1);
            if !CALLBACK_READERS_SATURATED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                logline!(
                    "clauth: callback reader refused: {} live at the {cap}-reader cap; the connection is dropped",
                    live - 1
                );
            }
            return None;
        }
        if live <= cap / 2 {
            CALLBACK_READERS_SATURATED.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        Some(Self { counter })
    }
}

impl Drop for CallbackReaderSlot {
    fn drop(&mut self) {
        self.counter.fetch_sub(1);
    }
}

/// Run one accepted callback connection's reader under the live-reader cap.
/// A refusal — the cap reached, or the OS refusing a thread — drops `reader`
/// and, with it, the captured stream: a flood costs connections, never a panic
/// in the login worker (`std::thread::spawn` panics on refusal).
pub(crate) fn spawn_callback_reader(reader: impl FnOnce() + Send + 'static) {
    let Some(slot) = CallbackReaderSlot::acquire() else {
        return;
    };
    // On a failed spawn the closure (and its slot) drops with the error, so
    // the count never leaks.
    if let Err(e) = std::thread::Builder::new()
        .name("clauth-callback".to_string())
        .spawn(move || {
            let _slot = slot;
            reader();
        })
    {
        logline!("clauth: callback reader thread not spawned: {e}; the connection is dropped");
    }
}

/// Read one callback request from `stream` until `done(&buf)` holds, the head
/// reaches `max_bytes`, the peer closes, or the connection's WHOLE `deadline`
/// passes. The socket's read timeout is re-armed to the time REMAINING before
/// every read, so a peer trickling one byte just under any per-read bound still
/// ends at the deadline, not after days (`daemon::bridge::read_chunk`, SB-10).
/// `None` on a read error, a stalled read, or the deadline.
pub(crate) fn read_callback_head(
    stream: &mut TcpStream,
    deadline: Instant,
    max_bytes: usize,
    done: impl Fn(&[u8]) -> bool,
) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        stream.set_read_timeout(Some(remaining)).ok()?;
        match stream.read(&mut chunk) {
            Ok(0) => return Some(buf),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if done(&buf) || buf.len() >= max_bytes {
                    return Some(buf);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

/// The "this login's wait is over" flag every reader tests before writing its
/// success page, plus the guard that raises it when `wait_for_code` returns
/// (every path, the error ones included). A reader accepted before the wait
/// ended can still finish afterwards; without the flag it would announce
/// success for a login that already aborted. A reader that read the flag clear
/// can still write its page if the wait ends in the microseconds before its
/// write; closing that would need a lock around the write.
pub(crate) struct WaitOver(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl WaitOver {
    pub(crate) fn new() -> (std::sync::Arc<std::sync::atomic::AtomicBool>, Self) {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        (flag.clone(), Self(flag))
    }
}

impl Drop for WaitOver {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

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
///
/// Shared with the Alibaba console login and its gateway calls, which encode
/// query values and form bodies against the same rule — a second copy would be
/// a second thing to get wrong.
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

/// Inverse of [`percent_encode`] for the callback query values: `+` → space, a
/// `%` not followed by two hex digits stays literal, and the bytes are read
/// lossily, so a callback that is not UTF-8 still yields a value to compare.
pub(crate) fn percent_decode(s: &str) -> String {
    // `Malformed::Keep` never refuses; the default is the type's other arm.
    let bytes = percent_decode_bytes(s, true, Malformed::Keep).unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// What a `%` not followed by two hex digits does to a decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Malformed {
    /// The `%` is copied through as a literal byte (a form value).
    Keep,
    /// The whole decode is refused (a path segment).
    Refuse,
}

/// The one `%XX` scan behind [`percent_decode`] and the REST router's path
/// decoder, which differ only in policy: whether `+` reads as a space and what
/// a malformed `%` does. Decodes from bytes with hex-digit validation: slicing
/// the `&str` by byte index (as `from_str_radix(&s[i+1..i+3])` would) panics
/// when a multi-byte UTF-8 char follows a bare `%`, reachable from any process
/// that hits the loopback port. `None` only under [`Malformed::Refuse`].
pub(crate) fn percent_decode_bytes(
    s: &str,
    plus_is_space: bool,
    malformed: Malformed,
) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => match bytes.get(i + 1..i + 3).and_then(hex_pair) {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None if malformed == Malformed::Keep => {
                    out.push(b'%');
                    i += 1;
                }
                None => return None,
            },
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(out)
}

/// The byte two hex digits spell; `None` for anything else. `from_str_radix`
/// alone would take a leading `+` or `-`, so the digits are checked first.
fn hex_pair(hex: &[u8]) -> Option<u8> {
    if hex.len() != 2 || !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
}

/// Percent-decoded value of `key` in an `a=1&b=2` query string. Also serves the
/// Alibaba console callback, whose params arrive in a body of the same shape.
pub(crate) fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// PKCE S256 challenge = base64url(sha256(verifier)).
fn challenge_from_verifier(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64url_nopad(&hasher.finalize())
}

/// `n` CSPRNG bytes, base64url-encoded — used for the verifier and `state`.
fn random_b64url(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("CSPRNG failure: {e}"))?;
    Ok(base64url_nopad(&buf))
}

/// A fresh `(code_verifier, code_challenge)` PKCE pair. 32 random bytes →
/// 43-char verifier, within RFC 7636's 43..128 range.
fn new_pkce() -> Result<(String, String)> {
    let verifier = random_b64url(32)?;
    let challenge = challenge_from_verifier(&verifier);
    Ok((verifier, challenge))
}

/// Build the authorize URL. `code=true` is appended unconditionally, exactly as
/// the Claude Code binary does for every authorize request (loopback and manual
/// alike) — it selects the CLI code flow; the loopback redirect still fires.
fn authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?code=true&client_id={cid}&response_type=code&redirect_uri={ru}\
         &scope={scope}&code_challenge={cc}&code_challenge_method=S256&state={state}",
        cid = percent_encode(crate::oauth::CLIENT_ID),
        ru = percent_encode(redirect_uri),
        // Claude Code form-encodes the scope separators as `+`, not `%20`;
        // every scope token is itself unreserved-safe
        // apart from its colons, which `percent_encode` still renders as `%3A`.
        scope = percent_encode(SCOPES).replace("%20", "+"),
        cc = percent_encode(challenge),
        state = percent_encode(state),
    )
}

/// The request target (path?query) from an HTTP request line: `GET <target> HTTP/1.1`.
fn request_target(request_line: &str) -> Option<&str> {
    let mut parts = request_line.split_whitespace();
    let _method = parts.next()?;
    parts.next()
}

/// Progress milestones reported through the login's callback. The CLI's paste
/// prompt ends on the first one (a door landed); the TUI login modal renders
/// both as a live stage line and takes the door off the first.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LoginProgress {
    /// The code arrived through this door; exchanging it for tokens.
    ExchangingCode(LoginMethod),
    /// Tokens minted; probing the plan tier to confirm they work.
    Verifying,
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

/// Why the authorize callback came back without a code — the browser-redirect
/// twin of [`crate::oauth::TokenFailure`], typed for the same reason.
///
/// The callback's `error` / `error_description` query params are upstream text
/// with NO cap (worse than the token-endpoint bodies, which at least passed a
/// first-line + 200-char trim), and they used to be interpolated straight into
/// `clauth login`'s stderr and the TUI Setup toast. So `error` is PARSED into
/// RFC 6749 §4.1.2.1's closed code set at the boundary and its bytes dropped;
/// `error_description` is free-form and never leaves the wire at all. No
/// `Display`, no conversion into `anyhow::Error`.
///
/// An unrecognized code is anonymous rather than echoed-with-a-cap: `logline!`
/// is line-oriented and nothing strips newlines, so echoing free upstream text
/// even into a log forges entries.
pub(crate) enum AuthorizeRejection {
    /// `access_denied` — the operator declined the consent screen. The one arm
    /// that is a choice rather than a failure.
    Declined,
    /// Anthropic's own side; a retry can clear it.
    Upstream(&'static str),
    /// The authorize REQUEST was refused, so a retry sends the same thing again.
    Refused(&'static str),
    /// An `error` value outside the spec's set — the case where the bytes are
    /// least trustworthy, so none are kept.
    Unrecognized,
}

impl AuthorizeRejection {
    /// Parse the callback's `error` param. The input is discarded here: every
    /// value any arm carries onward is one of this function's own literals, so
    /// nothing downstream can be holding browser-supplied bytes.
    pub(crate) fn parse(code: &str) -> Self {
        match code {
            "access_denied" => Self::Declined,
            "server_error" => Self::Upstream("server_error"),
            "temporarily_unavailable" => Self::Upstream("temporarily_unavailable"),
            "invalid_request" => Self::Refused("invalid_request"),
            "unauthorized_client" => Self::Refused("unauthorized_client"),
            "unsupported_response_type" => Self::Refused("unsupported_response_type"),
            "invalid_scope" => Self::Refused("invalid_scope"),
            _ => Self::Unrecognized,
        }
    }

    pub(crate) fn user_message(&self) -> &'static str {
        match self {
            Self::Declined => "you declined the authorization request",
            Self::Upstream(_) => "anthropic is having trouble",
            Self::Refused(_) | Self::Unrecognized => "anthropic refused the login",
        }
    }

    /// Operator-log rendering: the spec code the user copy withholds, as our own
    /// literal rather than the browser's bytes.
    pub(crate) fn log_detail(&self) -> &'static str {
        match self {
            Self::Declined => "access_denied",
            Self::Upstream(code) | Self::Refused(code) => code,
            Self::Unrecognized => "an error code outside RFC 6749's set (withheld)",
        }
    }
}

/// Handle one connection. `Ok(Some(code))` on the real `/callback` with matching
/// `state`; `Ok(None)` for any unrelated request (keep waiting); `Err` on an
/// OAuth error param or a state mismatch (a security stop).
///
/// `deadline` is the connection's WHOLE bound, armed by [`wait_for_code`] at
/// accept: a read that stalls or a peer trickling bytes past it ends the
/// connection here. `wait_over` is the login's flag: once the wait ended
/// (another door, or an abort), a reader finishing late writes NO page of any
/// kind — every page would lie about a login that is already over.
fn handle_callback(
    mut stream: TcpStream,
    expected_state: &str,
    deadline: Instant,
    wait_over: &std::sync::atomic::AtomicBool,
) -> Result<Option<String>> {
    // A read failure (half-open browser preconnect, a probe that stalls past the
    // connection deadline) must NOT abort the whole login — ignore this
    // connection and keep waiting. Only an OAuth `error` param or a state
    // mismatch is fatal; the overall timeout is enforced by the deadline in
    // `wait_for_code`.
    let Some(bytes) = read_callback_head(&mut stream, deadline, MAX_CALLBACK_HEAD_BYTES, |buf| {
        buf.contains(&b'\n')
    }) else {
        return Ok(None);
    };
    if wait_over.load(std::sync::atomic::Ordering::Acquire) {
        // The login already ended on another door or an aborted reader: write
        // NO page of any kind. The connection closes when `stream` drops.
        return Ok(None);
    }
    let request_line = String::from_utf8_lossy(&bytes);

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
    if path != "/callback" {
        write_response(
            &stream,
            "404 Not Found",
            Page {
                tone: Tone::Warning,
                title: "Nothing at this address",
                detail: "The login callback arrives at /callback on its own. \
                         You can close this tab.",
                auto_close: false,
            },
        );
        return Ok(None);
    }
    if let Some(err) = query_param(query, "error") {
        // Parse before anything else touches it: `err` and the `error_description`
        // beside it are uncapped browser-supplied text, and interpolating them
        // into the `bail!` below put them on `clauth login`'s stderr and the TUI
        // Setup toast verbatim. `error_description` is not read at all.
        let rejection = AuthorizeRejection::parse(&err);
        // A user-declined consent screen reads differently from a broken flow.
        let page = if matches!(rejection, AuthorizeRejection::Declined) {
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
                detail: "Claude reported an error during authorization. Close this \
                         tab and retry the login from clauth.",
                auto_close: false,
            }
        };
        write_response(&stream, "400 Bad Request", page);
        logline!(
            "clauth: the authorize callback refused the login: {}",
            rejection.log_detail()
        );
        anyhow::bail!("{}", rejection.user_message());
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

/// Accept loop until a code arrives through either door or `deadline` passes.
/// The paste door is polled first: `Ok` wins it, `Disconnected` closes it (the
/// listener keeps waiting), `Empty` falls through to the loopback accept.
/// Every accepted connection is read on its own reader thread under one
/// whole-connection [`callback_read_timeout`] deadline armed at accept, so a
/// silent or trickling socket never blocks the accept loop behind it; the first
/// reader's valid code wins, and a reader's `Err` (an OAuth `error` param or a
/// state mismatch) ends the wait with that error.
/// Non-`/callback` requests (favicon probes) are answered and ignored.
fn wait_for_code(
    listener: &TcpListener,
    paste: &std::sync::mpsc::Receiver<ManualCode>,
    expected_state: &str,
    deadline: Instant,
) -> Result<(String, LoginMethod)> {
    listener.set_nonblocking(true)?;
    let (tx, rx) = std::sync::mpsc::channel::<Result<Option<String>>>();
    let (wait_over, _wait_over_guard) = WaitOver::new();
    let mut paste_open = true;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for the login code (browser callback or pasted code, {LOGIN_TIMEOUT_SECS}s)"
            );
        }
        if paste_open {
            match paste.try_recv() {
                Ok(code) => return Ok((code.0, LoginMethod::Manual)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => paste_open = false,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // A reader thread's outcome. `tx` stays alive in this loop, so the
        // channel is never disconnected here; the arm only keeps the match
        // exhaustive over `TryRecvError`.
        match rx.try_recv() {
            Ok(Ok(Some(code))) => return Ok((code, LoginMethod::Browser)),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => return Err(e),
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let tx = tx.clone();
                let state = expected_state.to_string();
                let over = wait_over.clone();
                // Fixed at ACCEPT: the reader's budget is this connection's,
                // never re-armed by a later byte.
                let connection_deadline = Instant::now() + callback_read_timeout();
                spawn_callback_reader(move || {
                    stream.set_nonblocking(false).ok();
                    let _ = tx.send(handle_callback(stream, &state, connection_deadline, &over));
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(anyhow::Error::from(e).context("loopback accept failed")),
        }
    }
}

/// Build `ClaudeCredentials` from a minted token pair. `subscriptionType` is not
/// in the token response (Claude Code re-derives it), so it starts `None` here and
/// is stamped by `finish_login` from a live `/profile` probe.
fn credentials_from_token(token: crate::oauth::TokenResponse) -> ClaudeCredentials {
    let scopes = token
        .scope
        .as_deref()
        .map(|s| s.split_whitespace().map(String::from).collect());
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: token.access_token,
            refresh_token: Some(token.refresh_token),
            expires_at: Some((now_ms() + token.expires_in * 1000) as i64),
            scopes,
            subscription_type: None,
            // A login clauth mints itself has no outside-written keys to keep.
            // Claude Code adds its own on its first token save (measured:
            // `refreshTokenExpiresAt`, `rateLimitTier`), and the catch-all
            // holds them from then on. The tier is stamped HERE too instead of
            // waiting for that save: Claude Code reads it at STARTUP, before
            // any save, to evaluate plan-gated flags (#78).
            ..OAuthToken::default_extra()
        }),
    }
}

/// A completed interactive login: the minted credentials plus the account uuid
/// the verification probe saw them authenticate as. `account_uuid` is `None` when
/// the probe failed or the body carried no usable uuid — the login still stands,
/// and the anchor is simply left to the hourly ride-along backfill. `plan` is the
/// probe body's own plan (`None` when the probe failed), the seed a commit writes
/// into `usage_cache.json` so the tier shows at once.
#[derive(Debug, Clone)]
pub(crate) struct LoginOutcome {
    pub(crate) credentials: ClaudeCredentials,
    pub(crate) account_uuid: Option<AccountId>,
    pub(crate) plan: Option<PlanInfo>,
}

/// Why a `clauth login` produced no credential.
///
/// Typed so its two callers can diverge exactly as the switch path's already do:
/// `clauth login`'s stderr names the HTTP status (a terminal has no companion
/// log open beside it) and the TUI's login toast does not. No `Display` and no
/// `Into<anyhow::Error>`, so neither caller can bypass that split with a bare
/// `{e}` or a `?`.
pub(crate) enum LoginError {
    /// The token endpoint refused the authorization-code exchange — the one
    /// login failure that carries an HTTP status, so the only arm where the two
    /// renderings differ at all.
    Exchange(crate::oauth::TokenFailure),
    /// Everything else: the authorize callback's rejection (already canned by
    /// [`AuthorizeRejection`], and a browser redirect carries no HTTP status of
    /// ours to name), CSPRNG, the loopback bind/accept, the state mismatch, the
    /// login timeout. All clauth-authored, so both renderings coincide.
    Local(anyhow::Error),
}

impl LoginError {
    /// The LOGIN path's retry mapping, deliberately not
    /// [`crate::oauth::TokenFailure::as_refresh_transient`].
    ///
    /// The operative fact is that this function has NO retry path around
    /// `exchange_code`: whatever the status, the failure unwinds out of
    /// [`PendingLogin::run`] and the only action left to anyone is running
    /// `clauth login` again. So the refresh path's `Wait` — right for a 429 it
    /// will re-attempt on its next tick — names an action that does not exist
    /// here.
    ///
    /// Deliberately NOT justified by "the code is spent" or "the listener is
    /// torn down": the first is true of a 400 and not of a 429 (which likely
    /// never consumed the code), and the second is simply false — `listener` is
    /// still in scope through the exchange and drops only on unwind. Both were
    /// written here and both were wrong; the absence of a retry loop is the
    /// claim that is checkable and that stops being true if one is added.
    ///
    /// Only the transport arm keeps its own advice, because an unreachable
    /// endpoint is the one blocker worth clearing before running the command
    /// again.
    fn transient(f: &crate::oauth::TokenFailure) -> crate::format::Transient {
        use crate::format::{Cause, Retry, Transient};
        use crate::oauth::TokenFailure;
        let cause = Cause::Endpoint(f.user_message());
        match f {
            TokenFailure::Transport => Transient::new(cause, Retry::Connection),
            TokenFailure::Status(s) => Transient::with_status(cause, *s, Retry::Restart),
            TokenFailure::Body { status, .. } => {
                Transient::with_status(cause, *status, Retry::Restart)
            }
        }
    }

    /// Canned: the TUI login toast.
    pub(crate) fn user_message(&self) -> String {
        match self {
            Self::Exchange(f) => Self::transient(f).text(),
            Self::Local(e) => e.to_string(),
        }
    }

    /// Canned plus the HTTP status where one exists: `clauth login`'s stderr.
    pub(crate) fn cli_message(&self) -> String {
        match self {
            Self::Exchange(f) => Self::transient(f).text_with_status(),
            Self::Local(e) => e.to_string(),
        }
    }
}

/// The tail both doors share once an authorization code is in hand: exchange
/// it at the token endpoint, then verify the mint with one `/profile` probe.
/// `door` is the one that delivered `code`, and `redirect_uri` MUST be the one
/// that door's authorize request carried (the loopback URL or
/// [`MANUAL_REDIRECT_URI`]); the token endpoint rejects a mismatch.
fn finish_login(
    code: &str,
    verifier: &str,
    door: LoginMethod,
    redirect_uri: &str,
    state: &str,
    progress: &impl Fn(LoginProgress),
) -> std::result::Result<LoginOutcome, LoginError> {
    progress(LoginProgress::ExchangingCode(door));
    let token = crate::oauth::exchange_code(code, verifier, redirect_uri, state).map_err(|e| {
        // The BODY stops here; the status rides the typed value so stderr can
        // name it and the toast cannot.
        logline!("clauth: login code exchange failed: {}", e.log_detail());
        LoginError::Exchange(e)
    })?;
    let mut creds = credentials_from_token(token);

    progress(LoginProgress::Verifying);
    // One `/profile` round trip carries all of it: confirm the minted token works
    // against the API, stamp the real plan tier so the captured profile shows e.g.
    // "Claude Max" immediately instead of the unknown-tier "Pro" fallback, stamp
    // the rate-limit tier Claude Code would have written itself (#78: it feeds
    // the flag targeting that decides whether Fable is plan-included or
    // credits-only, and a login block without it reads as untiered), and carry
    // out the account uuid so the caller can anchor the profile without a
    // second identical request. Best-effort: a probe failure never fails the login
    // — clauth's usage poll re-derives the tier within a cycle and the anchor
    // backfills on the hourly ride-along.
    let mut account_uuid = None;
    let mut plan = None;
    if let Some(oauth) = creds.claude_ai_oauth.as_mut()
        && let Ok(probe) = crate::usage::probe_login_profile(&oauth.access_token)
    {
        oauth.subscription_type = probe.subscription_type;
        if let Some(tier) = probe.rate_limit_tier {
            oauth.set_rate_limit_tier(tier);
        }
        account_uuid = probe.account_uuid;
        plan = Some(probe.plan);
    }
    Ok(LoginOutcome {
        credentials: creds,
        account_uuid,
        plan,
    })
}

// ── one login, two doors ─────────────────────────────────────────────────────

/// What a login shows and checks against; nothing secret (the verifier never
/// leaves [`PendingLogin`]).
#[derive(Debug, Clone)]
pub(crate) struct LoginLinks {
    /// The loopback authorize URL for a browser on this host.
    pub(crate) browser_url: String,
    /// The hosted authorize URL ([`MANUAL_REDIRECT_URI`]) for any other device.
    pub(crate) hosted_url: String,
    /// The `state` a pasted code must carry.
    pub(crate) state: String,
}

impl LoginLinks {
    /// [`parse_manual_code`] against this login's `state`, wrapped in the
    /// [`ManualCode`] newtype with the typed error kept.
    pub(crate) fn parse(&self, pasted: &str) -> std::result::Result<ManualCode, ManualCodeError> {
        parse_manual_code(pasted, &self.state).map(ManualCode)
    }
}

/// A login minted and bound, not yet waiting. Made on the caller's thread (no
/// network); [`PendingLogin::run`] blocks on whichever thread the caller picks.
pub(crate) struct PendingLogin {
    verifier: String,
    listener: TcpListener,
    redirect_uri: String,
    links: LoginLinks,
}

/// Mint ONE PKCE pair and ONE `state`, bind `127.0.0.1:0`, and build both URLs
/// through [`authorize_url`]. The only failure is the CSPRNG or the bind.
pub(crate) fn begin_login() -> std::result::Result<PendingLogin, LoginError> {
    let (verifier, challenge) = new_pkce().map_err(LoginError::Local)?;
    let state = random_b64url(32).map_err(LoginError::Local)?;

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("failed to bind the loopback listener for the OAuth callback")
        .map_err(LoginError::Local)?;
    let port = listener
        .local_addr()
        .map_err(anyhow::Error::from)
        .map_err(LoginError::Local)?
        .port();
    let redirect_uri = format!("http://localhost:{port}/callback");
    let links = LoginLinks {
        browser_url: authorize_url(&redirect_uri, &challenge, &state),
        hosted_url: authorize_url(MANUAL_REDIRECT_URI, &challenge, &state),
        state,
    };

    Ok(PendingLogin {
        verifier,
        listener,
        redirect_uri,
        links,
    })
}

impl PendingLogin {
    /// Both authorize URLs and the `state` a pasted code must carry.
    pub(crate) fn links(&self) -> &LoginLinks {
        &self.links
    }

    /// Wait for the first door (the loopback callback, or a [`ManualCode`] on
    /// `paste`), then exchange with that door's redirect. Fires
    /// [`LoginProgress::ExchangingCode`] naming the door that won, then
    /// [`LoginProgress::Verifying`]. Never opens a browser: the caller does
    /// that with [`LoginLinks::browser_url`].
    pub(crate) fn run(
        self,
        paste: std::sync::mpsc::Receiver<ManualCode>,
        progress: impl Fn(LoginProgress),
    ) -> std::result::Result<LoginOutcome, LoginError> {
        let deadline = Instant::now() + Duration::from_secs(LOGIN_TIMEOUT_SECS);
        let (code, door) = wait_for_code(&self.listener, &paste, &self.links.state, deadline)
            .map_err(LoginError::Local)?;
        let redirect_uri = match door {
            LoginMethod::Browser => self.redirect_uri.as_str(),
            LoginMethod::Manual => MANUAL_REDIRECT_URI,
        };
        finish_login(
            &code,
            &self.verifier,
            door,
            redirect_uri,
            &self.links.state,
            &progress,
        )
    }
}

// ── the pasted code ──────────────────────────────────────────────────────────

/// An authorization code that passed [`LoginLinks::parse`] against the login's
/// own `state`. Redacted `Debug`: until exchanged it is worth exactly what the
/// token pair will be.
#[derive(Clone)]
pub(crate) struct ManualCode(String);

impl std::fmt::Debug for ManualCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ManualCode(..)")
    }
}

#[cfg(test)]
impl ManualCode {
    /// The code's bytes, for a test to check what a paste delivered.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a pasted string was refused. Canned text only, so no rendering of one
/// can carry the pasted bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManualCodeError {
    Empty,
    TooLong,
    /// No `#`, or an empty half on either side of it.
    Shape,
    /// The `state` half is not this login's: the paste came from a different
    /// run (or somebody else's URL). Same refusal the loopback callback makes.
    StateMismatch,
}

impl ManualCodeError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Empty => "no code entered",
            Self::TooLong => {
                "that is far longer than a login code; paste only the code the page shows"
            }
            Self::Shape => "invalid code; paste the code again including the #",
            Self::StateMismatch => "state mismatch: code came from a different login",
        }
    }
}

/// Split a pasted `code#state` and check it belongs to the login that expects
/// `expected_state`. Mirrors Claude Code's `Paste code here` parser (split on
/// `#`, both halves required) plus the state check the loopback path already
/// makes. Pure and never logs, so it is pinned without a network.
pub(crate) fn parse_manual_code(
    raw: &str,
    expected_state: &str,
) -> Result<String, ManualCodeError> {
    // Trim before the cap: the surrounding whitespace is not the code, so a
    // code of exactly the cap plus its newline is not "too long".
    let raw = raw.trim();
    if raw.len() > MANUAL_CODE_MAX {
        return Err(ManualCodeError::TooLong);
    }
    if raw.is_empty() {
        return Err(ManualCodeError::Empty);
    }
    let Some((code, state)) = raw.split_once('#') else {
        return Err(ManualCodeError::Shape);
    };
    if code.is_empty() || state.is_empty() {
        return Err(ManualCodeError::Shape);
    }
    if state != expected_state {
        return Err(ManualCodeError::StateMismatch);
    }
    Ok(code.to_string())
}

/// A one-glance summary of a captured login for the `clauth login` CLI. Never
/// prints the tokens — just a sha256 prefix of the refresh token (proves it is
/// real and lets you confirm it differs from other profiles), the granted
/// scopes, the access-token expiry, and the plan. The plan line prefers the
/// login's own `/profile` probe over the token stamp: the probe answering is
/// itself proof the minted token works, and an Ultra organization stamps no
/// `subscription_type` at all.
pub(crate) fn login_summary(creds: &ClaudeCredentials, plan: Option<&PlanInfo>) -> String {
    let Some(oauth) = creds.claude_ai_oauth.as_ref() else {
        return "  (no OAuth block captured)".to_string();
    };
    let sha = oauth
        .refresh_token
        .as_deref()
        .map(|rt| {
            let mut hasher = Sha256::new();
            hasher.update(rt.as_bytes());
            hasher
                .finalize()
                .iter()
                .take(6)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        })
        .unwrap_or_else(|| "(none)".to_string());
    let scopes = oauth
        .scopes
        .as_deref()
        .map(|s| s.join(" "))
        .unwrap_or_default();
    let expiry = oauth
        .expires_at
        .map(|ms| format!("{}s from now", (ms - now_ms() as i64) / 1000))
        .unwrap_or_else(|| "(unknown)".to_string());
    // The login's `/profile` probe in `finish_login` answered this very mint,
    // so its plan needs no corroboration from the token stamp; that stamp stays
    // the fallback for a probe that classified nothing.
    let probed = plan
        .filter(|p| p.is_classified())
        .and_then(PlanInfo::display);
    let plan = match probed {
        Some(label) => format!("  plan: {label} (token verified against the API)"),
        // An unclassifiable claim still proves the token works, so echo it raw
        // rather than dropping the line or naming a tier the token never made.
        None => match oauth.subscription_type.as_deref() {
            Some(sub) => {
                let tier = crate::usage::PlanTier::from_subscription_type(Some(sub)).display();
                format!(
                    "  plan: {} (token verified against the API)",
                    tier.as_deref().unwrap_or(sub)
                )
            }
            None => "  plan: will populate on the first usage refresh".to_string(),
        },
    };
    format!(
        "  refresh sha256: {sha}...\n  scopes: {scopes}\n  access token expires: {expiry}\n{plan}"
    )
}

#[cfg(test)]
#[path = "../tests/inline/oauth_login.rs"]
mod tests;
