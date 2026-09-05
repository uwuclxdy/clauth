//! Interactive browser OAuth login for a fresh Claude Code account, shared by
//! the `clauth login` CLI and the TUI Setup tab (login / re-login rows). Both
//! observe the flow through [`LoginProgress`] callbacks.
//!
//! Reproduces the Claude Code `/login` PKCE + RFC 8252 loopback flow so a new
//! profile can be populated from a real login instead of a snapshot. Ground truth
//! is the installed Claude Code binary (v2.1.199): the Pro/Max **subscription**
//! login authorizes at `claude.com/cai/oauth/authorize` (`CLAUDE_AI_AUTHORIZE_URL`
//! — the `platform.claude.com` host is the Console/API-billing surface and does
//! NOT mint claude.ai credentials), sends `code=true` plus the 6-scope set below,
//! and uses a loopback redirect to `http://localhost:<port>/callback`. The code is
//! then exchanged at `platform.claude.com/v1/oauth/token` via [`crate::oauth`].
//! The authorize-host risk knob is documented on [`AUTHORIZE_URL`].

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::logline::logline;
use crate::profile::{AccountId, ClaudeCredentials, OAuthToken};
use crate::usage::now_ms;

/// Claude Code's authorize endpoint for a Pro/Max **subscription** login
/// (`CLAUDE_AI_AUTHORIZE_URL` in v2.1.199). The `platform.claude.com/oauth/authorize`
/// host is Console/API-billing and does NOT mint claude.ai credentials — if a live
/// login 4xx's or shows an API-key consent screen, that host is the fallback knob.
const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";

/// The 6-scope union Claude Code requests for an interactive login (verbatim from
/// v2.1.199's `ALL_OAUTH_SCOPES`). `org:create_api_key` is Console-only but rides
/// the claude.ai path harmlessly; drop it first if authorize rejects the scope set.
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// How long to wait for the browser round-trip before giving up.
const LOGIN_TIMEOUT_SECS: u64 = 180;

/// Base64url without padding (RFC 4648 §5) — the encoding OAuth PKCE mandates.
fn base64url_nopad(input: &[u8]) -> String {
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

/// Progress milestones reported through `login_with`'s callback. The CLI
/// prints the authorize URL; the TUI login modal also renders the later
/// milestones as a live stage line.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LoginProgress<'a> {
    /// The authorize URL is built, just before the browser opens — surfaced so
    /// the flow is observable and the URL can be pasted if the open fails.
    AuthorizeUrl(&'a str),
    /// The loopback callback landed; exchanging the code for tokens.
    ExchangingCode,
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
    fn parse(code: &str) -> Self {
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

    fn user_message(&self) -> &'static str {
        match self {
            Self::Declined => "you declined the authorization request",
            Self::Upstream(_) => "anthropic is having trouble",
            Self::Refused(_) | Self::Unrecognized => "anthropic refused the login",
        }
    }

    /// Operator-log rendering: the spec code the user copy withholds, as our own
    /// literal rather than the browser's bytes.
    fn log_detail(&self) -> &'static str {
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
fn handle_callback(stream: TcpStream, expected_state: &str) -> Result<Option<String>> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut request_line = String::new();
    // A read failure (half-open browser preconnect, a probe that stalls past the
    // read timeout) must NOT abort the whole login — ignore this connection and
    // keep waiting. Only an OAuth `error` param or a state mismatch is fatal; the
    // overall timeout is enforced by the deadline in `wait_for_code`.
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

/// Accept loop until the callback arrives or `deadline` passes. Non-`/callback`
/// requests (favicon probes) are answered and ignored.
fn wait_for_code(
    listener: &TcpListener,
    expected_state: &str,
    deadline: Instant,
) -> Result<String> {
    listener.set_nonblocking(true)?;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for the browser login callback ({LOGIN_TIMEOUT_SECS}s)"
            );
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).ok();
                if let Some(code) = handle_callback(stream, expected_state)? {
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

/// Build `ClaudeCredentials` from a minted token pair. `subscriptionType` is not
/// in the token response (Claude Code re-derives it), so it starts `None` here and
/// is stamped by `login_with` from a live `/profile` probe.
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
        }),
    }
}

/// A completed interactive login: the minted credentials plus the account uuid
/// the verification probe saw them authenticate as. `account_uuid` is `None` when
/// the probe failed or the body carried no usable uuid — the login still stands,
/// and the anchor is simply left to the hourly ride-along backfill.
#[derive(Debug, Clone)]
pub(crate) struct LoginOutcome {
    pub(crate) credentials: ClaudeCredentials,
    pub(crate) account_uuid: Option<AccountId>,
}

/// Run the full interactive login: open the browser, catch the loopback
/// redirect, exchange the code, and return a completed [`LoginOutcome`].
/// `progress` receives [`LoginProgress`] milestones — the `AuthorizeUrl` event
/// fires just before opening the browser (the CLI prints it so the flow is
/// observable and the URL can be pasted if the browser doesn't open; the TUI
/// also renders the later stages). Opening the browser is best-effort: on
/// failure the announced URL is the fallback and the listener still waits.
/// Blocks the caller for the browser round-trip (up to [`LOGIN_TIMEOUT_SECS`]).
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
    /// browser timeout. All clauth-authored, so both renderings coincide.
    Local(anyhow::Error),
}

impl LoginError {
    /// The LOGIN path's retry mapping, deliberately not
    /// [`crate::oauth::TokenFailure::as_refresh_transient`].
    ///
    /// The operative fact is that this function has NO retry path around
    /// `exchange_code`: whatever the status, the failure unwinds out of
    /// `login_with` and the only action left to anyone is running `clauth login`
    /// again. So the refresh path's `Wait` — right for a 429 it will re-attempt
    /// on its next tick — names an action that does not exist here.
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

pub(crate) fn login_with(
    progress: impl Fn(LoginProgress<'_>),
) -> std::result::Result<LoginOutcome, LoginError> {
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
    let url = authorize_url(&redirect_uri, &challenge, &state);

    progress(LoginProgress::AuthorizeUrl(&url));
    let _ = crate::platform::open_url(&url);

    let deadline = Instant::now() + Duration::from_secs(LOGIN_TIMEOUT_SECS);
    let code = wait_for_code(&listener, &state, deadline).map_err(LoginError::Local)?;
    progress(LoginProgress::ExchangingCode);
    let token =
        crate::oauth::exchange_code(&code, &verifier, &redirect_uri, &state).map_err(|e| {
            // The BODY stops here; the status rides the typed value so stderr can
            // name it and the toast cannot.
            logline!("clauth: login code exchange failed: {}", e.log_detail());
            LoginError::Exchange(e)
        })?;
    let mut creds = credentials_from_token(token);

    progress(LoginProgress::Verifying);
    // One `/profile` round trip carries all of it: confirm the minted token works
    // against the API, stamp the real plan tier so the captured profile shows e.g.
    // "Claude Max" immediately instead of the unknown-tier "Pro" fallback, and
    // carry out the account uuid so the caller can anchor the profile without a
    // second identical request. Best-effort: a probe failure never fails the login
    // — clauth's usage poll re-derives the tier within a cycle and the anchor
    // backfills on the hourly ride-along.
    let mut account_uuid = None;
    if let Some(oauth) = creds.claude_ai_oauth.as_mut()
        && let Ok(probe) = crate::usage::probe_login_profile(&oauth.access_token)
    {
        oauth.subscription_type = probe.subscription_type;
        account_uuid = probe.account_uuid;
    }
    Ok(LoginOutcome {
        credentials: creds,
        account_uuid,
    })
}

/// A one-glance summary of a captured login for the `clauth login` CLI. Never
/// prints the tokens — just a sha256 prefix of the refresh token (proves it is
/// real and lets you confirm it differs from other profiles), the granted
/// scopes, and the access-token expiry.
pub(crate) fn login_summary(creds: &ClaudeCredentials) -> String {
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
    // The plan tier is stamped from a live `/profile` probe in `login_with`, so a
    // present value doubles as proof the minted token works against the API.
    let plan = match oauth.subscription_type.as_deref() {
        // An unclassifiable claim still proves the token works, so echo it raw
        // rather than dropping the line or naming a tier the token never made.
        Some(sub) => {
            let tier = crate::usage::PlanTier::from_subscription_type(Some(sub)).display();
            format!(
                "  plan: {} (token verified against the API)",
                tier.as_deref().unwrap_or(sub)
            )
        }
        None => "  plan: will populate on the first usage refresh".to_string(),
    };
    format!(
        "  refresh sha256: {sha}…\n  scopes: {scopes}\n  access token expires: {expiry}\n{plan}"
    )
}

#[cfg(test)]
#[path = "../tests/inline/oauth_login.rs"]
mod tests;
