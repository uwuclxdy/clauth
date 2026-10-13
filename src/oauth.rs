use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::claude::{LinkState, classify_credentials_link};
use crate::codex_auth::token_fingerprint;
use crate::lock::{StateLockHeld, with_state_lock};
use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::profile::{AccountId, AppConfig, ClaudeCredentials, OAuthToken, ProfileName, SlotOps};
use crate::runtime::RotationGuard;
use crate::usage::{
    ANTHROPIC_ORIGIN, ActivityStore, OpResult, OpResultSender, ProfileActivity, RefetchQueue,
    await_request_slot, clear_activity, mark_activity, now_ms,
};

/// OAuth token endpoint for BOTH the refresh and the interactive
/// authorization-code exchange — the host the current Claude Code binary uses
/// for each (verified on the wire: CC's axios refresh posts here, not to
/// `api.anthropic.com`). Paired with the `platform.claude.com` authorize host in
/// `oauth_login`.
const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";

/// Test-only [`TOKEN_ENDPOINT`] / [`MESSAGES_ENDPOINT`] overrides, so a loopback
/// listener can stand in for both and the rotation legs run offline. Without
/// them `fetch_with_rotation`, `auto_start_kick` and `rotate_one_inner` are
/// reachable by no test: each one's decision sits BEHIND an HTTP call, so a
/// refusal removed from any of them stays green. Serialized by
/// `profile::HOME_TEST_LOCK`, which every test that sets them already holds via
/// `HomeSandbox`. Never compiled into the binary.
#[cfg(test)]
static TOKEN_ENDPOINT_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(test)]
static MESSAGES_ENDPOINT_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_endpoint_overrides(token: &str, messages: &str) {
    if let Ok(mut guard) = TOKEN_ENDPOINT_OVERRIDE.lock() {
        *guard = Some(token.to_string());
    }
    if let Ok(mut guard) = MESSAGES_ENDPOINT_OVERRIDE.lock() {
        *guard = Some(messages.to_string());
    }
}

#[cfg(test)]
pub(crate) fn clear_endpoint_overrides() {
    if let Ok(mut guard) = TOKEN_ENDPOINT_OVERRIDE.lock() {
        *guard = None;
    }
    if let Ok(mut guard) = MESSAGES_ENDPOINT_OVERRIDE.lock() {
        *guard = None;
    }
}

fn token_endpoint() -> std::borrow::Cow<'static, str> {
    #[cfg(test)]
    {
        if let Some(url) = TOKEN_ENDPOINT_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
            return std::borrow::Cow::Owned(url);
        }
        panic!(
            "token endpoint requested with no test override; hold a `testutil::EndpointSandbox`"
        );
    }
    #[cfg(not(test))]
    {
        std::borrow::Cow::Borrowed(TOKEN_ENDPOINT)
    }
}

fn messages_endpoint() -> std::borrow::Cow<'static, str> {
    #[cfg(test)]
    {
        if let Some(url) = MESSAGES_ENDPOINT_OVERRIDE
            .lock()
            .ok()
            .and_then(|g| g.clone())
        {
            return std::borrow::Cow::Owned(url);
        }
        panic!(
            "messages endpoint requested with no test override; hold a `testutil::EndpointSandbox`"
        );
    }
    #[cfg(not(test))]
    {
        std::borrow::Cow::Borrowed(MESSAGES_ENDPOINT)
    }
}

/// `User-Agent` + `Accept` Claude Code's axios client sends on every token-endpoint
/// request. Mimicked so a refresh/exchange is byte-indistinguishable from CC's
/// (the version string is axios's, not ours, and will drift with CC's bundle).
pub(crate) const TOKEN_USER_AGENT: &str = "axios/1.15.2";
const TOKEN_ACCEPT: &str = "application/json, text/plain, */*";

/// Scopes echoed in the refresh `scope` field when a profile has none stored
/// (Claude Code sends its credential's granted scopes; this is that set for a
/// standard Pro/Max login, sans the Console-only `org:create_api_key`).
const REFRESH_SCOPES_FALLBACK: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Claude Code emits the refresh `scope` in this fixed order regardless of the
/// order its credential file happens to store the granted scopes in (verified on
/// the wire). A profile's stored `scopes` array is often
/// ordered differently, so reorder to this before sending to byte-match CC.
const CANONICAL_SCOPE_ORDER: [&str; 6] = [
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

/// Reorder a space-joined scope set into [`CANONICAL_SCOPE_ORDER`], appending any
/// unrecognized scope in its original position. Preserves the actual granted set
/// (never adds/drops a scope) — only the order changes.
fn canonicalize_scopes(scopes: &str) -> String {
    let present: Vec<&str> = scopes.split_whitespace().collect();
    let mut out: Vec<&str> = CANONICAL_SCOPE_ORDER
        .iter()
        .copied()
        .filter(|c| present.contains(c))
        .collect();
    out.extend(
        present
            .iter()
            .filter(|s| !CANONICAL_SCOPE_ORDER.contains(s)),
    );
    out.join(" ")
}

/// UUID of the "Claude Code" OAuth application; required for refresh and the
/// interactive login (`oauth_login` builds the authorize URL with it).
pub(crate) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Minimal inference endpoint we use to "kick" the 5-hour usage window.
/// Token refresh alone does NOT start the timer — only a real `/v1/messages`
/// call does. Probing with `count_tokens`, `oauth/usage`, or session
/// endpoints all confirmed this experimentally. `?beta=true` matches the query
/// Claude Code puts on every messages request (verified on the wire).
#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "test builds panic in the endpoint resolver instead of reaching the production URL"
    )
)]
const MESSAGES_ENDPOINT: &str = "https://api.anthropic.com/v1/messages?beta=true";

/// The `anthropic-beta` set Claude Code sends on its launch WARMUP post to
/// `/v1/messages`, distinct from the single `oauth-2025-04-20` on `/usage` and
/// from the longer lists CC's real inference calls carry. Captured 2026-07-14
/// against CC 2.1.209, re-verified unchanged 2026-07-24 against CC 2.1.219;
/// drifts with CC's bundle, re-capture on a bump.
const KICK_ANTHROPIC_BETA: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05";

/// anthropic-sdk-js (stainless) version CC bundles (2.1.209, still 0.94.0 at
/// 2.1.219), sent verbatim on the kick so its client-instrumentation headers
/// match CC's. NOTE: this is a
/// deliberately *partial* stainless set (lang/runtime/package-version only) — a
/// real SDK client also sends `x-stainless-arch/os/runtime-version`, which are
/// host-derived (and clauth has no honest node runtime-version), so they stay
/// off. Drifts with CC's bundle.
const KICK_STAINLESS_PACKAGE_VERSION: &str = "0.94.0";

/// Cheapest available model — single token costs ~0.001¢.
const KICK_MODEL: &str = "claude-haiku-4-5-20251001";

/// OAuth tokens require the "Claude Code" system prefix or the server rejects
/// the call as an unauthorized non-CC inference.
const KICK_SYSTEM_PROMPT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Pause between the steps of the 401/429-recovery sequence (failed kick →
/// rotate → retry kick → usage re-fetch) so the API sees the rotated pair settle
/// instead of three back-to-back requests on the same chain.
const ROTATION_STEP_DELAY_MS: u64 = 2000;

#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_in: u64,
    #[serde(default)]
    pub(crate) scope: Option<String>,
}

/// Why a token-endpoint call failed, holding none of the endpoint's own bytes.
///
/// Deliberately implements NO `Display` (and no `std::error::Error`, and no
/// conversion into `anyhow::Error`): a bare `{e}` on this type does not
/// compile, so a toast, a `bail!`, or an MCP JSON `reason` cannot print
/// Anthropic's words even by accident. [`Self::user_message`] and
/// [`Self::log_detail`] are the only ways out and both are built from the
/// variant alone.
///
/// The rejected alternative was one humanize() every surface agrees to call —
/// which is what `format::refresh_transient` already is, and the manual-rotate
/// toast bypassed it from four hundred lines away. A convention that has failed
/// once here is not the containment; the missing `Display` is.
pub(crate) enum TokenFailure {
    /// The endpoint answered `>= 400`. Its body decided the terminal-vs-transient
    /// split ([`refresh_rejection_is_terminal`]) and is dropped at that decision:
    /// upstream prose (`invalid_grant`, an `invalid_request_error` envelope, a
    /// WAF challenge page) names nothing a user can act on, and it is what used
    /// to reach toasts verbatim.
    Status(u16),
    /// No status was ever seen — transport, TLS, timeout, a truncated read, or a
    /// request body that failed to encode.
    Transport,
    /// A **2xx** body that did not parse into [`TokenResponse`]. That body still
    /// holds the live access+refresh tokens, so neither it nor serde's `Display`
    /// (which echoes the offending scalar — a possible token substring) may
    /// leave this type; a leaked token is account takeover. The value-free
    /// channel below is what [`Self::log_detail`] renders, pinned by
    /// `token_parse_error_redacts_the_2xx_body`.
    Body {
        status: u16,
        kind: &'static str,
        line: usize,
        column: usize,
        len: usize,
    },
}

impl TokenFailure {
    /// What a user is told. No status and no body: neither is actionable, and
    /// the body is the leak. The operator-facing detail rides
    /// [`Self::log_detail`] into a `logline!`.
    pub(crate) fn user_message(&self) -> &'static str {
        match self {
            // Register borrowed from the shipped `anthropic is throttling usage
            // reads` (`tui/render/usage.rs`, copy-rework #28) so the two throttle
            // surfaces read alike instead of each inventing a word for it.
            Self::Status(429) => "anthropic is throttling requests",
            // "rejected" is only true of a 4xx: a 503 from a CDN in front of the
            // endpoint is not Anthropic rejecting anything.
            Self::Status(s) if *s >= 500 => "anthropic is having trouble",
            Self::Status(_) => "anthropic rejected the request",
            Self::Transport => "could not reach anthropic",
            Self::Body { .. } => "anthropic's reply was unreadable",
        }
    }

    /// The REFRESH path's transient value: canned cause, the status for the
    /// surfaces allowed to name it, and the next step a refresh warrants.
    ///
    /// Named for its path because the retry hint is NOT a property of the
    /// failure alone — it depends on what the caller can still do. A refresh is
    /// re-attempted on the next tick, so `Wait` is right here; a login has no
    /// next tick and its code is spent, so `oauth_login` maps the same statuses
    /// to `Restart` instead. A third caller must pick, not inherit.
    pub(crate) fn as_refresh_transient(&self) -> crate::format::Transient {
        use crate::format::{Cause, Retry, Transient};
        // `Cause::Endpoint` takes `&'static str`, which is exactly what
        // `user_message` returns — a response body is a runtime `String` and
        // structurally cannot be substituted here.
        let cause = Cause::Endpoint(self.user_message());
        match self {
            Self::Status(s) => Transient::with_status(cause, *s, Retry::Wait),
            // No status was ever seen, and the connection is the one thing the
            // operator can act on.
            Self::Transport => Transient::new(cause, Retry::Connection),
            Self::Body { status, .. } => Transient::with_status(cause, *status, Retry::Wait),
        }
    }

    /// The `logline!` rendering: the status and parse position the user text
    /// withholds, still without a byte of the response.
    pub(crate) fn log_detail(&self) -> String {
        match self {
            Self::Status(status) => format!("HTTP {status}"),
            Self::Transport => "no response".to_string(),
            Self::Body {
                status,
                kind,
                line,
                column,
                len,
            } => format!(
                "HTTP {status} but the body did not parse as a token response \
                 ({kind} at line {line}, column {column}); {len} bytes withheld \
                 (contains live credentials)"
            ),
        }
    }
}

/// Classify a failed [`TokenResponse`] deserialization into [`TokenFailure::Body`]
/// — taking `e` by reference so the serde error itself cannot be moved into the
/// result and carried onward.
fn token_parse_error(e: &serde_json::Error, status: u16, body_len: usize) -> TokenFailure {
    TokenFailure::Body {
        status,
        kind: match e.classify() {
            serde_json::error::Category::Io => "io",
            serde_json::error::Category::Syntax => "malformed json",
            serde_json::error::Category::Data => "unexpected shape",
            serde_json::error::Category::Eof => "truncated",
        },
        line: e.line(),
        column: e.column(),
        len: body_len,
    }
}

/// Connect deadline for every token/kick call [`AGENT`] makes.
const HTTP_CONNECT_SECS: u64 = 4;
/// Response-HEADER deadline for the same. An IDLE deadline, re-armed from `now`
/// before every wait, not a phase bound measured from the connect — see
/// [`TOKEN_HTTP_DEADLINES`] below for what that difference costs.
const HTTP_RECV_HEADERS_SECS: u64 = 15;

/// The two deadlines a token call carries, added. Named for the deadlines rather
/// than for a phase because neither spelling of "the time a call may spend" is
/// true of it: one term bounds a phase and the other does not. It bounds NO PHASE of a token call end to end, and
/// reading it as a ceiling is the mistake the doc below exists to prevent.
///
/// `timeout_connect` is a true phase bound — upstream's wording is "Max duration
/// for establishing the connection. For a TLS connection this includes opening
/// the socket and doing the TLS handshake."
///
/// `timeout_recv_response` is NOT, despite reading like one. ureq 3.4.0 re-arms it
/// from `now` before every wait (`CallTimings::next_timeout`, re-called inside the
/// receive loop), so it caps the gap between two header bytes, never the phase. A
/// server dribbling one header byte every 3 s ran 135 s to a 200 under exactly
/// this agent config, against a 15.3 s timeout on a server that sent nothing —
/// measured 2026-08-31, which is also what proves the deadline is armed at all.
///
/// Of ureq's five per-phase deadlines the other three are left at their `None`
/// default here — `timeout_resolve`, `timeout_send_request` and
/// `timeout_send_body`, the last live rather than hypothetical since a refresh
/// POSTs a body — and so are `timeout_recv_body` and `timeout_global`. So every
/// phase of the call is unbounded: DNS, the request send, header receipt and the
/// response body alike.
///
/// Named rather than left as two literals inside [`AGENT`] because a caller that
/// must OUTLAST a refresh derives its own deadline from it —
/// [`crate::runtime::ROTATION_LOCK_TIMEOUT`], which waits out a rotation holding
/// the per-profile flock across this window. Building the agent from the same two
/// terms is what keeps the two from drifting: retuning either moves the waiter
/// with it. The unbounded phases are named there too, as legs that constant
/// cannot cover.
pub(crate) const TOKEN_HTTP_DEADLINES: Duration =
    Duration::from_secs(HTTP_CONNECT_SECS + HTTP_RECV_HEADERS_SECS);

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(HTTP_CONNECT_SECS)))
        .timeout_recv_response(Some(Duration::from_secs(HTTP_RECV_HEADERS_SECS)))
        // ureq 3 defaults non-2xx to `Err(Error::StatusCode)`, which `kick`'s
        // error mapping collapsed into `KickError::Other` — making the
        // 401 → rotate-and-retry leg unreachable. With the flag off, `kick`
        // reads the status from the `Ok` response and `refresh` checks it
        // explicitly below.
        .http_status_as_error(false)
        .build()
        .into()
});

/// The shared HTTP agent — one connect/recv budget and one
/// status-as-value policy for every clauth-side token call, the codex
/// refresh included.
pub(crate) fn http_agent() -> &'static ureq::Agent {
    &AGENT
}

/// A token-refresh failure, split so the AUTH-1 gate can tell a *permanently*
/// revoked/invalid refresh token (quarantine the account — `clauth login` is the
/// only fix) from a *transient* network/429/5xx blip (refuse this one switch,
/// retry next tick — never quarantine a healthy account on a hiccup).
///
/// No `From<RefreshError> for anyhow::Error`: that conversion collapsed the
/// split back into one opaque error AND smuggled the endpoint's raw body past
/// the classification into whatever surface caught it. Every caller now matches
/// the variant it cares about.
pub(crate) enum RefreshError {
    /// The endpoint confirmed the refresh token itself is dead — quarantine the
    /// account (`clauth login` is the only fix). See
    /// [`refresh_rejection_is_terminal`] for the status/body split.
    Invalid {
        failure: TokenFailure,
        /// The body was the flat OAuth2 envelope naming `invalid_grant`
        /// ([`names_invalid_grant`]): the server consumed or revoked this very
        /// token. False for a terminal 401 decided on status alone, which
        /// proves nothing about the token having been spent, so only a `true`
        /// lands in the spent record.
        invalid_grant: bool,
    },
    /// The refresh token may still be good: a transport failure, 429, 5xx, or a
    /// rejection the endpoint did not confirm as `invalid_grant`. Retry; never
    /// quarantine.
    Transient(TokenFailure),
}

impl RefreshError {
    /// The `logline!` rendering of either arm — the only place the endpoint's
    /// status still surfaces now that the user-facing text is canned.
    fn log_detail(&self) -> String {
        match self {
            Self::Invalid { failure: f, .. } | Self::Transient(f) => f.log_detail(),
        }
    }
}

/// The refresh request body CC's axios client posts to the token endpoint.
/// Pure so the exact wire JSON (field set + canonical `scope` order) is
/// golden-tested against the captured CC shape.
fn refresh_body(refresh_token: &str, scopes: Option<&str>) -> serde_json::Result<String> {
    serde_json::to_string(&serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
        "scope": canonicalize_scopes(scopes.unwrap_or(REFRESH_SCOPES_FALLBACK)),
    }))
}

/// The `authorization_code` exchange body (interactive login). Pure for the
/// same wire-parity golden test as [`refresh_body`].
fn exchange_body(
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    state: &str,
) -> serde_json::Result<String> {
    serde_json::to_string(&serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
        "code_verifier": code_verifier,
        "client_id": CLIENT_ID,
        "state": state,
    }))
}

/// [`refresh`] preserving the permanent-vs-transient distinction the AUTH-1 gate
/// needs. Terminal (quarantine) only when the endpoint confirms the refresh
/// token itself is dead; a transport error, 429, or 5xx is transient (retry,
/// never quarantine). See [`refresh_rejection_is_terminal`] for the split.
pub(crate) fn refresh_result(
    refresh_token: &str,
    scopes: Option<&str>,
) -> std::result::Result<TokenResponse, RefreshError> {
    let body = refresh_body(refresh_token, scopes)
        .map_err(|_| RefreshError::Transient(TokenFailure::Transport))?;

    let mut response = AGENT
        .post(token_endpoint().as_ref())
        .header("Content-Type", "application/json")
        .header("Accept", TOKEN_ACCEPT)
        .header("User-Agent", TOKEN_USER_AGENT)
        .send(&body)
        .map_err(|_| RefreshError::Transient(TokenFailure::Transport))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|_| RefreshError::Transient(TokenFailure::Transport))?;
    // `text` decides the split here and goes no further: [`TokenFailure`] has
    // nowhere to put it.
    if refresh_rejection_is_terminal(status, &text) {
        return Err(RefreshError::Invalid {
            failure: TokenFailure::Status(status),
            invalid_grant: names_invalid_grant(&text),
        });
    }
    if status >= 400 {
        return Err(RefreshError::Transient(TokenFailure::Status(status)));
    }

    serde_json::from_str(&text)
        .map_err(|e| RefreshError::Transient(token_parse_error(&e, status, text.len())))
}

/// Whether a token-endpoint rejection means the refresh token itself is dead
/// (quarantine) rather than the request being rejected or blocked (retry).
/// Extracted pure so the truth table is pinned offline
/// (`refresh_rejection_terminal_truth_table`).
///
/// A 400/403 needs the body to confirm `invalid_grant`. The endpoint answers a
/// dead token with the flat OAuth2 envelope, but reuses the same 400 for any
/// request it can't parse — with Anthropic's `invalid_request_error` envelope
/// instead. Quarantining on an unconfirmed 400 would flag every profile in the
/// chain the moment our own request shape drifts (a `client_id` bump, a scope
/// re-spelling), each recoverable only by a manual re-login; the same reasoning
/// already keeps a WAF/geo 403 out of quarantine. 401 stays terminal on status
/// alone: the endpoint never uses it for a live token, and a proxy that answers
/// one for a dead token carries no body to confirm.
fn refresh_rejection_is_terminal(status: u16, body: &str) -> bool {
    match status {
        400 | 403 => body.contains("invalid_grant"),
        401 => true,
        _ => false,
    }
}

/// Whether a rejection body is the flat OAuth2 envelope whose `error` IS
/// `invalid_grant` — the endpoint's own answer for a dead or consumed refresh
/// token — rather than a page that merely contains the literal or a status with
/// no body. Stricter than [`refresh_rejection_is_terminal`]'s substring on
/// purpose: a quarantine lifts on a re-login, a spent-record entry never does.
/// Answers a bool so the parse error, which can echo body bytes, goes no
/// further.
fn names_invalid_grant(body: &str) -> bool {
    #[derive(Deserialize)]
    struct Envelope {
        error: String,
    }
    serde_json::from_str::<Envelope>(body).is_ok_and(|e| e.error == "invalid_grant")
}

/// A profile's stored granted scopes, space-joined, for the refresh `scope`
/// field — read under the config lock and returned owned so no lock is held
/// across the HTTP refresh. `None` (→ [`REFRESH_SCOPES_FALLBACK`]) for an
/// unknown profile or one without stored scopes. Callers must not already hold
/// the config lock.
pub(crate) fn stored_scopes(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
) -> Option<String> {
    config.lock().ok()?.find(name)?.scopes_joined()
}

/// Exchange an authorization code (from the interactive login in `oauth_login`,
/// whether the loopback callback or a pasted manual code delivered it) for an
/// OAuth token pair. Uses the same client + HTTP agent as [`refresh_result`],
/// against [`TOKEN_ENDPOINT`] (the `platform.claude.com` host the current
/// Claude Code binary uses), carrying the same axios-mimicking headers.
/// `redirect_uri` MUST byte-match the one sent to the authorize endpoint (the
/// loopback URL, or `oauth_login::MANUAL_REDIRECT_URI`), and `state` echoes
/// the value round-tripped through the browser.
///
/// Errs as [`TokenFailure`] rather than `anyhow::Error` so the rejection body —
/// which reached a login toast and `clauth login`'s stderr verbatim — has
/// nowhere to ride.
pub(crate) fn exchange_code(
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    state: &str,
) -> std::result::Result<TokenResponse, TokenFailure> {
    let body = exchange_body(code, code_verifier, redirect_uri, state)
        .map_err(|_| TokenFailure::Transport)?;

    let mut response = AGENT
        .post(token_endpoint().as_ref())
        .header("Content-Type", "application/json")
        .header("Accept", TOKEN_ACCEPT)
        .header("User-Agent", TOKEN_USER_AGENT)
        .send(&body)
        .map_err(|_| TokenFailure::Transport)?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|_| TokenFailure::Transport)?;
    if status >= 400 {
        return Err(TokenFailure::Status(status));
    }

    serde_json::from_str(&text).map_err(|e| token_parse_error(&e, status, text.len()))
}

/// A kick failure. Distinguishes a 401 (access token expired — rotate the chain
/// and retry) from every other failure (body encode, transport, or any non-401
/// HTTP status), which is terminal for this attempt. Mirrors `FetchError::Status`
/// so the auto-start rotation leg reacts to the same signal the fetch path does.
///
/// Carries no `Display` and no conversion into `anyhow::Error` (the latter
/// existed only to give a test a panic string, and was the same smuggling shape
/// [`RefreshError`] documents): a kick failure can only be rendered through
/// [`describe_kick_failure`].
enum KickError {
    /// The Messages endpoint returned this >=400 status; a 429 carries the
    /// limiter's own metadata when the response held any.
    Status(u16, Option<KickRateLimit>),
    /// Body encode or transport failure before a status was seen. [`kick_to`]
    /// never reads a response BODY, so one cannot arrive here — but a ureq
    /// transport error can still echo a server-supplied HEADER (`ureq_proto`'s
    /// `BadLocationHeader` Display's the raw `Location` value), so treat this as
    /// log-only rather than as clauth-authored text.
    Other(anyhow::Error),
}

/// Operator-log rendering of a kick failure, for the diagnostic `logline!` when
/// a kick dies on something the recovery paths don't handle (non-401/429 status,
/// transport, body encode). Never a notification surface: `logline!` writes the
/// daemon log or `~/.clauth/clauth.log`, which is where the status belongs now
/// that user-facing copy withholds it. Pure so the mapping is unit-testable
/// without HTTP.
fn describe_kick_failure(err: &KickError) -> String {
    match err {
        KickError::Status(status, _) => format!("HTTP {status}"),
        KickError::Other(e) => e.to_string(),
    }
}

/// What the messages limiter said alongside a kick 429. `until_epoch_secs` is
/// the advertised retry ceiling — the later of
/// `anthropic-ratelimit-unified-reset` and `retry-after` — and is an UPPER
/// BOUND only: the limiter has been observed relenting 2.4h before its own
/// advertised reset (2026-07-15), so callers retry with
/// decay toward it, never sleep until it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KickRateLimit {
    /// `anthropic-ratelimit-unified-status: rejected` — the account-level hard
    /// rejection, as opposed to a plain burst throttle.
    pub(crate) rejected: bool,
    pub(crate) until_epoch_secs: Option<i64>,
}

/// Distill a kick 429's rate-limit headers. Pure so the parse is testable
/// without HTTP; `now_secs` anchors the relative `retry-after` form and drops
/// an already-past advertised reset.
fn kick_rate_limit_at(
    unified_status: Option<&str>,
    unified_reset: Option<&str>,
    retry_after: Option<&str>,
    now_secs: i64,
) -> KickRateLimit {
    let reset = unified_reset
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&t| t > now_secs);
    // Strictly-future only, like `reset` above: `retry-after: 0` mapping to a
    // now-ceiling would collapse the backoff clamp to "always due" and re-kick
    // every tick — the trap `next_slot_deferral` already guards on `/usage`.
    let after = retry_after
        .and_then(|v| crate::usage::parse_retry_after_at(v, now_secs))
        .map(|d| now_secs.saturating_add(i64::try_from(d.as_secs()).unwrap_or(i64::MAX)))
        .filter(|&t| t > now_secs);
    KickRateLimit {
        rejected: unified_status.is_some_and(|s| s.eq_ignore_ascii_case("rejected")),
        until_epoch_secs: reset.max(after),
    }
}

/// Sends a 1-token Haiku message to start the 5-hour usage window. Mirrors what
/// Claude Code does silently on launch. Shares the `api.anthropic.com` per-host
/// request-spacing slot so a same-instant multi-profile window-reset doesn't burst
/// `/v1/messages`.
fn kick(access_token: &str) -> std::result::Result<(), KickError> {
    kick_to(messages_endpoint().as_ref(), access_token)
}

/// The kick's actual work, with the target `url` parameterized so a loopback
/// listener can pin the emitted header set (`kick_emits_cc_message_wire_shape`).
/// Carries Claude Code's `/v1/messages` client shape — the SDK instrumentation +
/// full beta set CC sends — minus the per-session headers
/// (`x-claude-code-session-id`, `x-client-request-id`) clauth has no honest value
/// for, and the host-derived `x-stainless-arch/os/runtime-version` (see
/// [`KICK_STAINLESS_PACKAGE_VERSION`]). The `system` prefix stays: an OAuth token
/// without it is rejected as non-CC inference.
fn kick_to(url: &str, access_token: &str) -> std::result::Result<(), KickError> {
    await_request_slot(ANTHROPIC_ORIGIN);
    let body = serde_json::to_string(&serde_json::json!({
        "model": KICK_MODEL,
        "max_tokens": 1,
        "system": [{ "type": "text", "text": KICK_SYSTEM_PROMPT }],
        "messages": [{ "role": "user", "content": "x" }],
    }))
    .map_err(|e| KickError::Other(e.into()))?;

    let response = AGENT
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", KICK_ANTHROPIC_BETA)
        .header("anthropic-dangerous-direct-browser-access", "true")
        .header("User-Agent", crate::usage::cli_user_agent())
        .header("x-app", "cli")
        .header("x-stainless-lang", "js")
        .header("x-stainless-runtime", "node")
        .header(
            "x-stainless-package-version",
            KICK_STAINLESS_PACKAGE_VERSION,
        )
        .send(&body)
        .map_err(|e| KickError::Other(anyhow::Error::from(e)))?;
    let status = response.status().as_u16();
    if status >= 400 {
        let rate_limit = (status == 429).then(|| {
            let header = |k: &str| response.headers().get(k).and_then(|v| v.to_str().ok());
            kick_rate_limit_at(
                header("anthropic-ratelimit-unified-status"),
                header("anthropic-ratelimit-unified-reset"),
                header("retry-after"),
                crate::usage::now_epoch_secs(),
            )
        });
        return Err(KickError::Status(status, rate_limit));
    }
    Ok(())
}

/// Outcome of an [`auto_start_kick`]. `opened` is whether the 5h window opened
/// (a 2xx from the messages endpoint, first try or post-rotation retry).
/// `rotated` carries a freshly minted `(access, refresh)` pair whenever a
/// rotation happened; the pair is live even when `opened` is false, because the
/// previous single-use refresh token is already spent and dropping it would
/// strand the profile.
#[must_use]
pub(crate) struct KickResult {
    pub(crate) opened: bool,
    pub(crate) rotated: Option<(String, Option<String>)>,
    /// The limiter's metadata when the deciding failure was a 429 (first kick
    /// or the post-rotation retry) — what the scheduler's block state and the
    /// TUI pill are built from.
    pub(crate) blocked: Option<KickRateLimit>,
    /// The rotation's persist refused because the store holds no login any
    /// more (a logout landed mid-rotation): the caller drops its token-list
    /// entry rather than poll on a gone login.
    pub(crate) login_gone: bool,
}

impl KickResult {
    fn not_opened() -> Self {
        Self::not_opened_with(None)
    }

    fn not_opened_with(blocked: Option<KickRateLimit>) -> Self {
        Self {
            opened: false,
            rotated: None,
            blocked,
            login_gone: false,
        }
    }
}

/// Fire the 1-token Haiku ping that opens a profile's 5h window. On a 401
/// (expired access token) it rotates the chain once and retries. On a 429
/// (rate-limited) it rotates ONLY when `access_expires_at` is in the past — a
/// clock-expired token is the one case where a refresh could actually unstick
/// the kick. A 429 on a still-valid token is a pure endpoint rate limit a
/// refresh can't fix; rotating it would spend the single-use refresh token every
/// 60s tick under a sustained 429 (the steady-state fetch path refuses 429
/// rotation entirely for exactly this reason). Unknown expiry (`None`) is
/// treated as not-expired, so it does not rotate.
///
/// Same double-spend guard as `fetch_with_rotation`'s rotation leg:
/// `RotationGuard` outermost across the refresh HTTP window, and the rotated
/// pair returned to the caller for the live token snapshot. A first kick that
/// succeeds spends only the access token and takes no `RotationGuard`.
///
/// Each recovery step is paced by [`ROTATION_STEP_DELAY_MS`] (kick → rotate →
/// retry kick → caller's usage re-fetch); none of the sleeps holds the rotation
/// lock. `activity` (the scheduler's store) drives the spinner; the CLI passes
/// `None`.
pub(crate) fn auto_start_kick(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    access_token: &str,
    refresh_token: Option<&str>,
    access_expires_at: Option<i64>,
    activity: Option<&ActivityStore>,
) -> KickResult {
    let first_rl = match kick(access_token) {
        Ok(()) => {
            return KickResult {
                opened: true,
                rotated: None,
                blocked: None,
                login_gone: false,
            };
        }
        Err(KickError::Status(401, _)) => None,
        // Rate limit (429): rotate only if the access token is also clock-expired;
        // a still-valid token can't be unstuck by a refresh, so refuse to spend it.
        Err(KickError::Status(429, rl))
            if access_expires_at.is_some_and(|exp| now_ms() as i64 >= exp) =>
        {
            rl
        }
        Err(KickError::Status(429, rl)) => return KickResult::not_opened_with(rl),
        // Every other first-kick failure is terminal for this attempt and used to
        // vanish here — name the real status/error so a persistently-dead ping
        // (e.g. a rejecting 403) is diagnosable instead of completely silent.
        Err(e) => {
            logline!(
                "{name}: 5h window kick failed: {}",
                describe_kick_failure(&e)
            );
            return KickResult::not_opened();
        }
    };

    let Some(rt) = refresh_token else {
        return KickResult::not_opened_with(first_rl);
    };
    // Pace the recovery before any lock is taken.
    std::thread::sleep(std::time::Duration::from_millis(ROTATION_STEP_DELAY_MS));
    // RotationGuard outermost across the HTTP window — acquired with no other
    // lock held (the caller released the usage store before kicking).
    let Ok(rotation_guard) = RotationGuard::acquire(name) else {
        return KickResult::not_opened_with(first_rl);
    };
    // macOS only: clauth can't write the Keychain item this session's CC reads,
    // so rotating would sign it out (`runtime::rotation_blocked_by_live_session`).
    if crate::runtime::rotation_blocked_for(name) {
        return KickResult::not_opened_with(first_rl);
    }
    let rt = match send_rule(name, rt, &rotation_guard) {
        SendRule::Send(rt) => rt,
        // The store's newer login is what the caller's token list follows; the
        // window opens on a later tick with it.
        SendRule::Adopt(pair) => {
            if adopt_disk_rotation(config, name, &rotation_guard).is_err() {
                return KickResult::not_opened_with(first_rl);
            }
            return KickResult {
                opened: false,
                rotated: Some(pair),
                blocked: first_rl,
                login_gone: false,
            };
        }
        SendRule::Quarantine => {
            mark_auth_broken(config, name, true);
            return KickResult::not_opened_with(first_rl);
        }
        SendRule::Skip => return KickResult::not_opened_with(first_rl),
    };

    // Refresh spinner during the round trip, then back to Fetching for the retry
    // kick + the caller's fetch (the kick runs inside the scheduler's fetch leg).
    if let Some(activity) = activity {
        mark_activity(activity, name, ProfileActivity::Refreshing);
    }
    let refreshed = settle_refresh_answer(
        config,
        name,
        &rt,
        refresh_result(&rt, stored_scopes(config, name).as_deref()),
        &rotation_guard,
    );
    if let Some(activity) = activity {
        // This site raised `Refreshing`, so it is the one that may retire it.
        crate::usage::rotation_into_fetch(activity, name);
    }
    let minted = match refreshed {
        Ok(minted) => minted,
        Err(_) => return KickResult::not_opened_with(first_rl),
    };

    let (access, new_refresh) = minted.pair();
    // The refresh already spent the old single-use token, so this pair is now the
    // only usable one — carry it back even when the persist below fails, or the
    // caller's live snapshot keeps the dead token and 400s every tick. The retry
    // kick may still fail (`opened` false), but a minted pair must always
    // propagate (see `KickResult`) — unless the persist REFUSED, when the store's
    // pair is what the caller follows.
    let rotated = Some((access.clone(), new_refresh));
    match apply_rotated_tokens_locked(
        config,
        name,
        &minted.sent_fp,
        &minted.creds,
        &minted.old_access,
    ) {
        Ok(()) => {}
        Err(PersistError::Refused { store }) => {
            let carried = PersistError::carried_pair(store.as_deref());
            return KickResult {
                opened: false,
                login_gone: carried.is_none(),
                rotated: carried,
                blocked: first_rl,
            };
        }
        Err(PersistError::Failed(_)) => {
            return KickResult {
                opened: false,
                rotated,
                blocked: first_rl,
                login_gone: false,
            };
        }
    }
    // Retry kick spends only the access token, so release the rotation lock
    // before the paced waits — a sibling worker shouldn't block on our sleeps.
    drop(rotation_guard);

    // Pace rotate → retry kick, then retry kick → the caller's usage re-fetch.
    std::thread::sleep(std::time::Duration::from_millis(ROTATION_STEP_DELAY_MS));
    let (opened, retry_rl) = match kick(&access) {
        Ok(()) => (true, None),
        Err(KickError::Status(429, rl)) => (false, rl),
        Err(e) => {
            logline!(
                "{name}: 5h window retry kick failed after rotation: {}",
                describe_kick_failure(&e)
            );
            (false, None)
        }
    };
    std::thread::sleep(std::time::Duration::from_millis(ROTATION_STEP_DELAY_MS));
    KickResult {
        opened,
        rotated,
        blocked: if opened { None } else { retry_rl.or(first_rl) },
        login_gone: false,
    }
}

/// Result of [`rotate_one_inner`]. Distinguishes the rotation-lock acquire
/// failure (no `OpResult` emitted, no activity pre-stamp to clear) from every
/// other path (which emits its own `OpResult` and clears activity). Lets
/// `refresh_all` workers surface the guard-fail as a Danger toast.
/// The ONE spelling for "this profile's rotation lock could not be taken",
/// shared by both `OpResult` legs and the pre-install switch gate. Three call
/// sites, one `Cause` arm: `format.rs` exists because this exact condition used
/// to print a different sentence per surface, and `5391a4c` re-created that by
/// rewording the gate's copy while leaving the two toasts on their own string.
fn rotation_lock_unavailable(name: &ProfileName) -> crate::format::Transient {
    crate::format::Transient::new(
        crate::format::Cause::RotationLockUnavailable(name.to_string()),
        // The cause names its own next step; a second one contradicts it.
        crate::format::Retry::Stated,
    )
}

/// CLA-ROLL: a rolling-token sidecar could not be written or restored. The
/// chain is fine; the file in front of it is not. See
/// [`crate::format::Cause::SidecarWriteFailed`].
fn sidecar_write_failed(name: &ProfileName) -> crate::format::Transient {
    crate::format::Transient::new(
        crate::format::Cause::SidecarWriteFailed(name.to_string()),
        // The cause names its own next step; a second one contradicts it.
        crate::format::Retry::Stated,
    )
}

/// CLA-ROLL: map a failed sidecar repair to its Transient — contention and
/// fault are different verdicts. The repair bodies run under
/// `with_state_lock`, which fails on a bounded cross-process flock timeout
/// ([`crate::lock::StateLockTimeout`]), and on macOS that flock is held
/// across `/usr/bin/security` shell-outs sharing a 20 s aggregate budget
/// (`lock::SUBPROCESS_BUDGET`, each invocation capped at 10 s) — so a slow
/// Keychain in a SIBLING process surfaces here as a timeout, and rendering it
/// through [`sidecar_write_failed`]'s "check permissions" copy sends the
/// operator hunting a fault that does not exist. Same contention-vs-fault
/// split as `RotationLockUnavailable` (round 1) and `RotationLockHeld`
/// (round 3), one lock further down.
fn sidecar_repair_transient(name: &ProfileName, e: &anyhow::Error) -> crate::format::Transient {
    if e.chain()
        .any(|c| c.downcast_ref::<crate::lock::StateLockTimeout>().is_some())
    {
        return crate::format::Transient::new(
            crate::format::Cause::StateLockBusy(name.to_string()),
            crate::format::Retry::Wait,
        );
    }
    sidecar_write_failed(name)
}

/// CLA-ROLL: a live `clauth start` session is holding the ROTATING pair,
/// because it started before the sidecar was armed. See
/// [`crate::format::Cause::LiveSessionOnRotatingChain`].
fn live_session_on_rotating_chain(name: &ProfileName) -> crate::format::Transient {
    crate::format::Transient::new(
        crate::format::Cause::LiveSessionOnRotatingChain(name.to_string()),
        crate::format::Retry::Stated,
    )
}

/// CLA-ROLL: another holder has the rotation lock and this caller must not
/// park behind it. See [`crate::format::Cause::RotationLockHeld`].
fn rotation_lock_held(name: &ProfileName) -> crate::format::Transient {
    crate::format::Transient::new(
        crate::format::Cause::RotationLockHeld(name.to_string()),
        // The cause names its own next step; a second one contradicts it.
        crate::format::Retry::Stated,
    )
}

/// CLA-ROLL: the chain's recorded grant cannot be told from a mint, so the
/// roll is refused. See [`crate::format::Cause::RollingGrantUnrecorded`].
fn rolling_grant_unrecorded(name: &ProfileName) -> crate::format::Transient {
    crate::format::Transient::new(
        crate::format::Cause::RollingGrantUnrecorded(name.to_string()),
        // Only a re-login fixes it, and the cause says so.
        crate::format::Retry::Stated,
    )
}

/// CLA-ROLL: the sidecar holds a rotating pair with nothing live to heal it,
/// and this caller must not fall into the blocking vanilla gate. See
/// [`crate::format::Cause::SidecarMisfilled`].
fn sidecar_misfilled(name: &ProfileName) -> crate::format::Transient {
    crate::format::Transient::new(
        crate::format::Cause::SidecarMisfilled(name.to_string()),
        // Only a re-capture fixes it, and the cause says so.
        crate::format::Retry::Stated,
    )
}

enum RotateOutcome {
    /// `RotationGuard::acquire` failed — the lock file could not be created or
    /// opened. NOT contention: `acquire` blocks on the flock, so a sibling
    /// worker or a live session holding it makes this leg wait rather than
    /// arrive here. No `OpResult` was emitted.
    GuardUnavailable,
    /// The HTTP/persist leg ran and emitted its `OpResult`. The bool is whether
    /// the rotated pair was persisted.
    Persisted(bool),
}

/// Whether `env` carries an api key. An env-carried `ANTHROPIC_API_KEY` is a
/// key (owner ruling 2026-09-02: env-key keeps dead-chain), so the splitter
/// renders the keyless sentence only where neither the field
/// ([`crate::claude::has_usable_api_key`]) nor the env holds one — the
/// `[env]`-token shape. Same key and trim-non-empty test as the env half of
/// [`crate::claude::has_inference_auth`], which matches both env keys at
/// once, so no exact helper exists to reuse.
fn env_has_api_key(env: &BTreeMap<String, String>) -> bool {
    env.get("ANTHROPIC_API_KEY")
        .is_some_and(|v| !v.trim().is_empty())
}

/// How a profile's dead chain reads when the chain is not the whole of what it
/// has, or `None` when the caller's own `login_expired` rendering applies. The
/// one place that split is decided — the rotate toast, the quarantine's own
/// log line and `clauth rolling-token`'s bail all route through it, so no two
/// of them can prescribe different commands for one state.
///
/// The two arms carry `mcp::preflight_target`'s two predicates. The ORDER is
/// the opposite one and can be, because these arms are disjoint where the
/// gate's overlap: a keyless profile fails `has_own_inference_endpoint` too, so
/// the gate has to refuse it for the key BEFORE reaching its quarantine arm,
/// while here the own-endpoint arm already excludes it. An account serving its
/// own inference is told the split state
/// whether or not clauth recognises its provider (a dead chain beside a
/// working key reads the same on litellm as on DeepSeek), and a RECOGNISED
/// keyless one is told about the key. A keyless unrecognised endpoint falls
/// through to `None` on purpose — it may be a local model needing no key, the
/// same 2026-08-28 ruling that keeps the delegate's keyless arm scoped.
///
/// The own-endpoint arm consults the durable `AuthExpired` verdict before
/// rendering the split state: when the record matches the profile's CURRENT
/// credential (fingerprint via [`crate::usage::profile_credential_fingerprint`],
/// read by [`crate::profile_cache::auth_expired_matches`]), the arm renders the
/// sentence true for what the verdict measured. Everywhere except Alibaba the
/// verdict pronounces the api key dead, so the keyless sentence renders.
/// Alibaba's verdict records a dead console session instead — its usage fetch
/// never reads the api key — so the dead-console sentence renders there, and
/// neither sibling does. That arm additionally requires a console to have been
/// captured: the verdict collapses "never captured" into "dead", which is right
/// for fetch scheduling and wrong for copy, so a console-less Alibaba profile
/// keeps the dead-chain sentence. The consult needs a credential to fingerprint,
/// so a profile holding none ([`crate::claude::has_usable_api_key`] false and no
/// env-carried `ANTHROPIC_API_KEY` — the `[env]`-token shape) skips it and
/// renders the keyless sentence: for that profile the sentence is literally
/// true, not a verdict's claim. An env-carried key keeps the split sentence
/// (owner ruling 2026-09-02).
pub(crate) fn third_party_dead_chain_copy(
    profile: Option<&crate::profile::Profile>,
    name: &ProfileName,
) -> Option<String> {
    let profile = profile?;
    if crate::claude::has_own_inference_endpoint(profile) {
        // A profile holding no usable api key has no key the split sentence
        // could claim still works — provided the env carries none either
        // (owner ruling 2026-09-02: an env-carried key keeps the split
        // sentence). The keyless sentence is literally true only for the
        // `[env]`-token shape, so render it there without a verdict (the
        // consult has no fingerprint to match for that shape anyway).
        if !crate::claude::has_usable_api_key(profile) && !env_has_api_key(&profile.env) {
            return Some(crate::format::third_party_keyless(name));
        }
        // A matching verdict retires the dead-chain sentence for every
        // provider; which replacement is true depends on what the verdict
        // measured. Everywhere except Alibaba it pronounces the api key dead,
        // so the keyless sentence renders. Alibaba's verdict records a dead
        // console session (its usage fetch never reads the api key), so the
        // dead-console sentence renders there — the keyless one would
        // mis-claim a live key, and the dead-chain one would name the wrong
        // half.
        //
        // The Alibaba arm needs a console to have EXISTED. `alibaba::fetch`
        // collapses "never captured" into "dead" deliberately, since neither
        // is worth a request, and the verdict inherits that collapse. Copy is
        // where the two states differ: "expired ... re-capture" is false for a
        // profile that never had one, so a console-less Alibaba profile keeps
        // the dead-chain sentence, which stays true of it.
        if crate::usage::profile_credential_fingerprint(profile)
            .is_some_and(|fp| crate::profile_cache::auth_expired_matches(name, fp))
        {
            return if profile.provider == Some(crate::providers::Provider::Alibaba) {
                if profile.console.is_some() {
                    Some(crate::format::third_party_dead_console(name))
                } else {
                    Some(crate::format::third_party_dead_chain(name))
                }
            } else {
                Some(crate::format::third_party_keyless(name))
            };
        }
        return Some(crate::format::third_party_dead_chain(name));
    }
    if profile.is_third_party() && !crate::claude::has_inference_auth(profile) {
        return Some(crate::format::third_party_keyless(name));
    }
    None
}

/// The dead-chain arm's toast detail.
fn dead_chain_detail(config: &crate::profile::ConfigHandle, name: &ProfileName) -> String {
    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    let cfg = config.lock().expect("config mutex poisoned");
    third_party_dead_chain_copy(cfg.find(name), name)
        .unwrap_or_else(|| crate::format::login_expired(name).detail().to_string())
}

/// Body of each [`refresh_all`] worker. Holds the per-profile rotation lock
/// across the ENTIRE HTTP window so an external `clauth start <name>` cannot
/// begin a refresh of the same single-use token while ours is in flight (the
/// state flock can't — it must release across the round trip). Ordering rule
/// (matches `ProfileRuntime::acquire`): RotationGuard OUTERMOST, then state
/// flock inside.
///
/// A live `clauth start` session is rotated like any other profile: it reads
/// the same `.credentials.json` this writes, so it picks the new pair up on its
/// next request rather than racing for the chain.
///
/// HTTP/persist leg emits one `OpResult { kind: Refreshing }` and clears the
/// activity slot. Returns [`RotateOutcome::GuardUnavailable`] without emitting an
/// `OpResult` when the lock can't be acquired (slot never pre-stamped here;
/// `refresh_all` pre-stamps and clears it). The no-refresh-token leg, and a
/// spent token whose store already holds a newer login (adopted, nothing
/// sent), return [`RotateOutcome::Persisted(false)`] silently.
fn rotate_one_inner(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    activity: Option<&ActivityStore>,
    sender: &OpResultSender,
) -> RotateOutcome {
    let Ok(rotation_guard) = RotationGuard::acquire(name) else {
        return RotateOutcome::GuardUnavailable;
    };
    let token = {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let cfg = config.lock().expect("config mutex poisoned");
        with_state_lock(|_held| {
            // macOS only: clauth can't write the Keychain item this session's CC
            // reads, so rotating would sign it out. Skipping returns
            // Persisted(false) (`runtime::rotation_blocked_by_live_session`).
            if crate::runtime::rotation_blocked_for(name) {
                return Ok::<_, anyhow::Error>(None);
            }
            let Some(rt) = cfg
                .find(name)
                .and_then(|p| p.refresh_token().map(str::to_string))
            else {
                return Ok(None);
            };
            // Granted scopes read under the SAME lock as the refresh token so the
            // refresh body echoes them exactly (matches Claude Code's wire shape).
            let scopes = cfg.find(name).and_then(|p| p.scopes_joined());
            if let Some(activity) = activity {
                // Stamp Refreshing under the state lock so partition_due cannot
                // observe this profile as Idle between the credential read and
                // the HTTP call. Lock order (AppConfig → state → leaf) is preserved:
                // activity is a leaf mutex acquired inside with_state_lock.
                mark_activity(activity, name, ProfileActivity::Refreshing);
            }
            Ok(Some((rt, scopes)))
        })
        .ok()
        .flatten()
    };

    let Some((rt, scopes)) = token else {
        return RotateOutcome::Persisted(false);
    };
    let outcome = match send_rule(name, &rt, &rotation_guard) {
        SendRule::Send(rt) => rotate_send(config, name, &rt, scopes.as_deref(), &rotation_guard),
        // The store already holds a newer login than the spent one in memory:
        // nothing to spend, nothing failed, so no toast.
        SendRule::Adopt(_) => match adopt_disk_rotation(config, name, &rotation_guard) {
            Ok(()) => {
                if let Some(activity) = activity {
                    clear_activity(activity, name);
                }
                return RotateOutcome::Persisted(false);
            }
            Err(e) => Err(anyhow::anyhow!("{}", adopt_lock_transient(name, &e).text())),
        },
        SendRule::Quarantine => {
            mark_auth_broken(config, name, true);
            Err(anyhow::anyhow!("{}", dead_chain_detail(config, name)))
        }
        SendRule::Skip => Err(anyhow::anyhow!("{}", spent_record_unreadable(name).text())),
    };
    let applied = outcome.is_ok();
    if let Some(activity) = activity {
        clear_activity(activity, name);
    }
    let _ = sender.send(OpResult {
        name: name.to_string(),
        outcome,
    });
    RotateOutcome::Persisted(applied)
}

/// [`rotate_one_inner`]'s spend: refresh `rt` and persist the answer.
fn rotate_send(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    rt: &str,
    scopes: Option<&str>,
    rotation_guard: &RotationGuard,
) -> Result<()> {
    // `refresh_result`, not a collapsing wrapper: this leg's `OpResult` becomes a
    // Danger toast, and a dead chain has a different next step from a network
    // blip. Flattening the split here is what put `HTTP 400: {"error":
    // "invalid_grant", …}` on screen while the switch gate and the poll — both
    // matching the variant — showed the canned line.
    let answer = refresh_result(rt, scopes);
    match settle_refresh_answer(config, name, rt, answer, rotation_guard) {
        Ok(minted) => apply_rotated_tokens_locked(
            config,
            name,
            &minted.sent_fp,
            &minted.creds,
            &minted.old_access,
        )
        .map_err(PersistError::into_error),
        Err(e) => {
            logline!("clauth: refresh for '{name}' failed: {}", e.log_detail());
            // This `OpResult`'s only sink is the TUI's Danger toast, whose first
            // line already reads `refresh for '<name>' failed` — so the OAuth
            // arm carries the NEXT STEP alone rather than restating the
            // condition and the account name under it. Both third-party
            // branches restate it: their sentences are owner-ruled copy,
            // rendered whole so the surfaces cannot drift.
            Err(match e {
                RefreshError::Invalid { .. } => {
                    anyhow::anyhow!("{}", dead_chain_detail(config, name))
                }
                RefreshError::Transient(f) => {
                    anyhow::anyhow!("{}", f.as_refresh_transient().text())
                }
            })
        }
    }
}

/// Profiles `refresh_all` would rotate, as `(name, refresh_token)` pairs.
/// Extracted so tests can pin the inclusion logic without the network.
/// Diverged-active profiles are included only when `force`. A live
/// `clauth start` session does not exclude a profile: it shares the credential
/// file a rotation writes, so it follows the new pair instead of being cut off
/// from one.
pub(crate) fn rotation_candidates(config: &AppConfig, force: bool) -> Vec<(ProfileName, String)> {
    // force=true (t-key rotate-all) bypasses diverged-active: user wants every
    // account rotated, including the one CC is touching.
    let skip_active = !force && active_link_diverged(config);
    config
        .profiles
        .iter()
        .filter_map(|p| {
            if skip_active && config.is_active(&p.name) {
                return None;
            }
            Some((p.name.clone(), p.refresh_token()?.to_string()))
        })
        .collect()
}

/// Refreshes every profile's OAuth token pair (rotated pair saved to disk).
/// Mirrors what Claude Code does silently on launch — minus the kick.
///
/// Profiles without a stored refresh token are skipped. Network/revocation
/// failures are swallowed per-profile; cached state stays put. `force`
/// bypasses only the diverged-active guard.
///
/// Returns the names whose rotation succeeded so the caller can target
/// follow-up work (re-fetch, kick) at the same set, and pushes each onto
/// `refetch` so the next tick re-fetches usage without waiting for the cadence.
///
/// Takes `&ConfigHandle` so per-profile workers lock/unlock independently around
/// their HTTP calls, never holding the config mutex across the network. Each
/// worker emits one `OpResult` on `sender` the moment its HTTP completes, so the
/// spinner clears in arrival order, not when the slowest sibling finishes.
pub(crate) fn refresh_all(
    config: &crate::profile::ConfigHandle,
    force: bool,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    sender: &OpResultSender,
) -> Vec<String> {
    let snapshots = {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let cfg = config.lock().expect("config mutex poisoned");
        rotation_candidates(&cfg, force)
    };

    if snapshots.is_empty() {
        return Vec::new();
    }

    // Stamp every candidate Refreshing before the fan-out so the overview row
    // shows a refresh spinner for the entire window. Each worker clears its
    // own slot when it emits its OpResult so the spinner drops as soon as
    // that profile's HTTP returns, not when the slowest sibling does.
    for (name, _) in &snapshots {
        mark_activity(activity, name, ProfileActivity::Refreshing);
    }

    // Pair each handle with the name so the join loop can clear the activity
    // slot on panic — the closure consumes the name, so we keep a second copy.
    let handles: Vec<(ProfileName, _)> = snapshots
        .into_iter()
        .map(|(name, _rt)| {
            let config = Arc::clone(config);
            let activity = Arc::clone(activity);
            let sender = sender.clone();
            let name_for_handle = name.clone();
            let h = std::thread::spawn(move || {
                // Holds the per-profile RotationGuard across the HTTP window so
                // an external `clauth start <name>` cannot double-spend this
                // single-use token mid-rotation.
                let outcome = rotate_one_inner(&config, &name, Some(&activity), &sender);
                (name, outcome)
            });
            (name_for_handle, h)
        })
        .collect();

    let mut refreshed = Vec::new();
    for (name, h) in handles {
        match h.join() {
            Ok((n, RotateOutcome::Persisted(true))) => refreshed.push(n.to_string()),
            // Guard-fail leg never emits an OpResult, so this pre-stamped slot
            // would freeze the spinner AND swallow the failure. Emit the Danger
            // toast (matches the pre-collapse worker) and clear.
            Ok((n, RotateOutcome::GuardUnavailable)) => {
                let _ = sender.send(OpResult {
                    name: n.to_string(),
                    outcome: Err(anyhow::anyhow!("{}", rotation_lock_unavailable(&n).text())),
                });
                clear_activity(activity, &n);
            }
            // Persist/skip legs already emitted their OpResult and cleared their
            // slot; a re-clear is idempotent and guards the skipped-no-token path.
            Ok((n, RotateOutcome::Persisted(false))) => clear_activity(activity, &n),
            Err(_) => {
                // Worker panicked before `clear_activity`. Clear here so the
                // spinner doesn't freeze and `any_busy` can resolve. No OpResult
                // was sent, so no toast for this profile.
                clear_activity(activity, &name);
            }
        }
    }
    if let Ok(mut q) = refetch.lock() {
        for name in &refreshed {
            q.insert(name.clone());
        }
    }
    refreshed
}

/// Rotate a single profile's OAuth token pair — one [`refresh_all`] worker leg,
/// scoped to `name` (the action-menu "rotate tokens" on the focused account).
/// Same discipline: `rotate_one_inner` holds the per-profile RotationGuard
/// across the HTTP window. On success the profile is pushed onto `refetch` so
/// the next tick re-fetches its usage. Returns `true` when a new pair
/// persisted.
pub(crate) fn rotate_one(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    sender: &OpResultSender,
) -> bool {
    // Pre-stamp so the row shows a refresh spinner for the whole HTTP window;
    // rotate_one_inner clears the slot when it emits its OpResult.
    mark_activity(activity, name, ProfileActivity::Refreshing);
    let persisted = match rotate_one_inner(config, name, Some(activity), sender) {
        RotateOutcome::Persisted(true) => true,
        // Guard-fail never emits an OpResult; surface the failure + clear, exactly
        // as refresh_all's join loop does for an unavailable guard.
        RotateOutcome::GuardUnavailable => {
            let _ = sender.send(OpResult {
                name: name.to_string(),
                outcome: Err(anyhow::anyhow!(
                    "{}",
                    rotation_lock_unavailable(name).text()
                )),
            });
            clear_activity(activity, name);
            false
        }
        // Persist/skip legs already emitted + cleared; clearing the pre-stamp again
        // is idempotent and covers the no-refresh-token early return.
        RotateOutcome::Persisted(false) => {
            clear_activity(activity, name);
            false
        }
    };
    if persisted && let Ok(mut q) = refetch.lock() {
        q.insert(name.to_string());
    }
    persisted
}

/// One-shot window prime for the CLI switch: if `name` is an opted-in OAuth
/// account, fire the kick (rotating once on a 401/429 via [`auto_start_kick`]).
/// No scheduler side channels and no cooldown — the CLI runs once and exits, so
/// there is no tick to debounce against. Returns whether the window opened.
///
/// The just-switched profile is active and freshly reconciled, so the diverged-
/// active guard the steady-state path needs doesn't apply here; opt-in + OAuth
/// is the whole gate.
pub(crate) fn prime_window(config: &crate::profile::ConfigHandle, name: &ProfileName) -> bool {
    let (access_token, refresh_token, expires_at) = {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let cfg = config.lock().expect("config mutex poisoned");
        match with_state_lock(|_held| {
            let Some(profile) = cfg.find(name) else {
                return Ok::<_, anyhow::Error>(None);
            };
            if !profile.is_oauth() || !profile.auto_start {
                return Ok(None);
            }
            let Some(token) = profile.access_token().map(str::to_string) else {
                return Ok(None);
            };
            let refresh = profile.refresh_token().map(str::to_string);
            Ok(Some((token, refresh, profile.access_token_expires_at())))
        }) {
            Ok(Some(t)) => t,
            _ => return false,
        }
    };

    let kicked = auto_start_kick(
        config,
        name,
        &access_token,
        refresh_token.as_deref(),
        expires_at,
        None,
    );
    if let Some(rl) = kicked.blocked {
        let ceiling = rl
            .until_epoch_secs
            .map(|u| {
                let left = u.saturating_sub(crate::usage::now_epoch_secs());
                format!(", api ceiling in {}", crate::usage::humanize_duration(left))
            })
            .unwrap_or_default();
        logline!(
            "{name}: 5h window kick rate-limited (rejected: {}){ceiling}",
            rl.rejected
        );
    }
    kicked.opened
}

// ── the claude refresh chain: spent record, stage, post-answer step ─────────
//
// A refresh token is single-use, so the pair a refresh mints is the chain head
// the moment the server answers, and a persist that then fails must not lose
// it. The post-answer step stages that pair (`credentials.json.staged`) and
// records the token the server consumed (`auth.spent.json`), both under the
// rotation guard and off the state flock. The persist then lands the pair
// through [`land_pair`], or refuses a store this chain does not own.

const SPENT_RECORD_FILE: &str = "auth.spent.json";
/// How many fingerprints the spent record keeps, newest last.
const SPENT_RECORD_CAP: usize = 64;
const STAGE_FILE: &str = "credentials.json.staged";

/// `profiles/<name>/auth.spent.json`: the fingerprints
/// ([`crate::codex_auth::token_fingerprint`]) of refresh tokens the server
/// answered for, oldest first, never a token. Read lock-free.
pub(crate) enum SpentRecord {
    /// The fingerprints on disk; empty when no record exists yet.
    Known(Vec<String>),
    /// A read or parse error other than an absent file: whether a token is
    /// spent is unknown, so nothing may be judged live off this record.
    Unreadable,
}

impl SpentRecord {
    pub(crate) fn read(name: &ProfileName) -> Self {
        let Ok(path) = crate::profile::profile_subpath(name, SPENT_RECORD_FILE) else {
            return Self::Unreadable;
        };
        Self::read_at(&path)
    }

    /// The record of the profile whose store is `store`: the readers that hold
    /// a store path rather than a name (the session watchdog's copy-back).
    pub(crate) fn beside(store: &std::path::Path) -> Self {
        match store.parent() {
            Some(dir) => Self::read_at(&dir.join(SPENT_RECORD_FILE)),
            None => Self::Unreadable,
        }
    }

    fn read_at(path: &std::path::Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_or(Self::Unreadable, Self::Known),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::Known(Vec::new()),
            Err(_) => Self::Unreadable,
        }
    }

    /// `Some(true)` when the server consumed `token`, `None` when the record
    /// cannot say.
    pub(crate) fn spent(&self, token: &str) -> Option<bool> {
        match self {
            Self::Known(fps) => Some(fps.contains(&token_fingerprint(token))),
            Self::Unreadable => None,
        }
    }
}

/// The profile's directory when it exists. The off-flock writers below write
/// only into it, so a rotation whose profile was deleted under a stale config
/// never recreates the directory with its own bookkeeping.
fn existing_profile_dir(name: &ProfileName) -> Option<std::path::PathBuf> {
    crate::profile::profile_dir(name)
        .ok()
        .filter(|dir| dir.is_dir())
}

/// Append `token`'s fingerprint to the spent record, keeping the newest
/// [`SPENT_RECORD_CAP`]. Never over an unreadable record, whose older entries
/// a rewrite would lose. Not fail-closed: the server already answered, so a
/// failed write is logged and the rotation goes on.
fn record_spent(name: &ProfileName, token: &str) {
    let Some(dir) = existing_profile_dir(name) else {
        return;
    };
    let SpentRecord::Known(mut fps) = SpentRecord::read(name) else {
        logline!(
            "clauth: '{name}': cannot read its spent-token record, so the refresh token just used was not recorded"
        );
        return;
    };
    let fp = token_fingerprint(token);
    // A re-answered token moves to the newest end, so the cap evicts by the
    // latest answer rather than the first.
    if fps.last() == Some(&fp) {
        return;
    }
    fps.retain(|f| f != &fp);
    fps.push(fp);
    let excess = fps.len().saturating_sub(SPENT_RECORD_CAP);
    fps.drain(..excess);
    let written = serde_json::to_vec(&fps)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| {
            crate::profile::atomic_write_600(&dir.join(SPENT_RECORD_FILE), bytes)
                .map_err(anyhow::Error::from)
        });
    if let Err(e) = written {
        logline!("clauth: '{name}': failed to record the refresh token just used: {e:#}");
    }
}

/// Retire an UNREADABLE spent record at a login install or clear, which would
/// otherwise hold every later spend of the new login off until a hand delete.
/// A readable record stays: an install can put back a pair the chain already
/// consumed (a capture of a live slot that lags the store), and the record is
/// the only fact that marks it spent.
pub(crate) fn retire_unreadable_spent_record(name: &ProfileName) {
    if matches!(SpentRecord::read(name), SpentRecord::Unreadable)
        && let Ok(path) = crate::profile::profile_subpath(name, SPENT_RECORD_FILE)
    {
        let _ = std::fs::remove_file(path);
    }
}

/// `profiles/<name>/credentials.json.staged`: a minted pair whose persist has
/// not landed. `base` is the fingerprint of the refresh token the store held
/// when the pair was staged, `base_access` that login's access token (the old
/// pair the post-persist hooks recognise), `creds` the whole login block the
/// store is to hold. 0600: it carries a live pair, like the store.
#[derive(Serialize, Deserialize)]
pub(crate) struct StagedPair {
    pub(crate) base: String,
    pub(crate) base_access: String,
    pub(crate) creds: ClaudeCredentials,
}

/// The three states every stage reader decides by ([`StagedPair::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageState {
    /// The store still holds the base and the staged pair is unspent: the
    /// staged pair is the chain head.
    Live,
    /// The store already holds the unspent staged pair, its hooks possibly
    /// unrun (a persist that died between its store write and its hooks, or
    /// a login of that same pair).
    Landed,
    /// The store moved past the base, or the staged pair is spent or
    /// unknown.
    Inert,
}

impl StagedPair {
    /// `None` for an absent or unparseable stage.
    pub(crate) fn read(name: &ProfileName) -> Option<Self> {
        let path = crate::profile::profile_subpath(name, STAGE_FILE).ok()?;
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    /// Judge this stage against the store's refresh token `store_refresh`
    /// (`None`: no store, or no login in it).
    pub(crate) fn state(&self, store_refresh: Option<&str>, record: &SpentRecord) -> StageState {
        let Some(staged) = self.creds.refresh_token() else {
            return StageState::Inert;
        };
        if record.spent(staged) != Some(false) {
            return StageState::Inert;
        }
        match store_refresh {
            Some(d) if token_fingerprint(d) == self.base => StageState::Live,
            Some(d) if d == staged => StageState::Landed,
            _ => StageState::Inert,
        }
    }
}

/// The live stage's login for a loader, given the store's raw login: read
/// lock-free and touching no file.
pub(crate) fn live_stage_credentials(
    name: &ProfileName,
    store: Option<&ClaudeCredentials>,
) -> Option<ClaudeCredentials> {
    let stage = StagedPair::read(name)?;
    let store_refresh = store.and_then(ClaudeCredentials::refresh_token);
    (stage.state(store_refresh, &SpentRecord::read(name)) == StageState::Live)
        .then_some(stage.creds)
}

fn delete_stage(name: &ProfileName) {
    if let Ok(path) = crate::profile::profile_subpath(name, STAGE_FILE) {
        let _ = std::fs::remove_file(path);
    }
}

/// The store's login read raw: `Ok(None)` for an absent store, `Err` for one
/// that exists and cannot be read or parsed.
pub(crate) fn read_raw_store(name: &ProfileName) -> Result<Option<ClaudeCredentials>> {
    let path = crate::profile::profile_subpath(name, "credentials.json")?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).map_err(|_| {
                anyhow::anyhow!("failed to parse {}", path.display())
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::from(e).context(format!("failed to read {}", path.display()))),
    }
}

/// What the post-answer step hands the persist.
pub(crate) struct Minted {
    /// The fingerprint of the refresh token this rotation sent.
    pub(crate) sent_fp: String,
    /// The store's login block with the answer's pair, absolute expiry and
    /// scope written in, so the persist writes exactly what was staged.
    pub(crate) creds: ClaudeCredentials,
    /// The access token of the login the store held at answer time: the old
    /// pair the post-persist hooks recognise in the live slot and Keychain.
    pub(crate) old_access: String,
}

impl Minted {
    /// The minted `(access, refresh)` pair, for a caller's token list.
    pub(crate) fn pair(&self) -> (String, Option<String>) {
        (
            self.creds.access_token().unwrap_or_default().to_string(),
            self.creds.refresh_token().map(str::to_string),
        )
    }
}

/// The post-answer step every spend site runs the moment the token endpoint
/// answers, still under the guard and before `config` or the state flock.
/// On a pair: stage it when the store is this chain's, then record the token
/// it consumed (stage first, so a crash between the two leaves a live stage
/// whose base the persist's base clause still honours). On a body-confirmed
/// `invalid_grant`: record the token. Anything else records nothing, so a
/// network blip leaves its token sendable.
pub(crate) fn settle_refresh_answer(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    sent: &str,
    answer: std::result::Result<TokenResponse, RefreshError>,
    _guard: &RotationGuard,
) -> std::result::Result<Minted, RefreshError> {
    let tok = match answer {
        Ok(tok) => tok,
        Err(e) => {
            if matches!(
                e,
                RefreshError::Invalid {
                    invalid_grant: true,
                    ..
                }
            ) {
                record_spent(name, sent);
            }
            return Err(e);
        }
    };
    // An unreadable store stages nothing: the persist refuses to guess at it.
    // The pair's block then comes from memory's own login, so the in-memory
    // move a failed persist makes keeps the plan and tier beside the minted
    // pair, never an empty block.
    let (store, block) = match read_raw_store(name) {
        Ok(store) => (store.clone(), store),
        Err(_) => (
            None,
            config
                .lock()
                .ok()
                .and_then(|cfg| cfg.find(name).and_then(|p| p.credentials.as_ref().cloned())),
        ),
    };
    let store_refresh = store.as_ref().and_then(ClaudeCredentials::refresh_token);
    let sent_fp = token_fingerprint(sent);
    let creds = minted_credentials(block, tok);
    let mut old_access = store
        .as_ref()
        .and_then(ClaudeCredentials::access_token)
        .unwrap_or_default()
        .to_string();
    if let Some(d) = store_refresh {
        let record = SpentRecord::read(name);
        let stage = match StagedPair::read(name)
            .filter(|g| g.state(Some(d), &record) == StageState::Live)
        {
            // The store has not moved since the live stage, so its base
            // carries forward unchanged.
            Some(live) => Some(StagedPair {
                base: live.base,
                base_access: live.base_access,
                creds: creds.clone(),
            }),
            None => (token_fingerprint(d) == sent_fp || record.spent(d) == Some(true)).then(|| {
                StagedPair {
                    base: token_fingerprint(d),
                    base_access: old_access.clone(),
                    creds: creds.clone(),
                }
            }),
        };
        if let Some(stage) = stage {
            old_access.clone_from(&stage.base_access);
            let written = match existing_profile_dir(name) {
                Some(dir) => serde_json::to_vec_pretty(&stage)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| {
                        crate::profile::atomic_write_600(&dir.join(STAGE_FILE), bytes)
                            .map_err(anyhow::Error::from)
                    }),
                None => Ok(()),
            };
            if let Err(e) = written {
                logline!(
                    "clauth: '{name}': failed to save the new token pair beside the store: {e:#}"
                );
            }
        }
    }
    // A pair proves `sent` consumed only when it rotated: an answer echoing the
    // same refresh token leaves it the live one.
    if creds.refresh_token() != Some(sent) {
        record_spent(name, sent);
    }
    Ok(Minted {
        sent_fp,
        creds,
        old_access,
    })
}

/// The store's login block (`store`, or an empty one) with `tok`'s pair,
/// expiry and scope written in: every other key of the block (the plan, the
/// rate-limit tier, Claude Code's own fields) is the store's.
fn minted_credentials(store: Option<ClaudeCredentials>, tok: TokenResponse) -> ClaudeCredentials {
    let mut creds = store.unwrap_or(ClaudeCredentials {
        claude_ai_oauth: None,
    });
    write_token_fields(
        creds
            .claude_ai_oauth
            .get_or_insert_with(OAuthToken::default_extra),
        tok,
    );
    creds
}

/// Write rotated token fields into an OAuth block, the expiry made absolute
/// once, here.
fn write_token_fields(oauth: &mut OAuthToken, tok: TokenResponse) {
    oauth.access_token = tok.access_token;
    oauth.refresh_token = Some(tok.refresh_token);
    oauth.expires_at = Some((now_ms() + tok.expires_in * 1000) as i64);
    if let Some(scope) = tok.scope {
        oauth.scopes = Some(scope.split_whitespace().map(String::from).collect());
    }
}

/// How [`land_pair`] resolved.
pub(crate) enum Landing {
    /// The store now holds the pair and the post-persist hooks ran.
    Landed,
    /// The store holds a login this chain does not own (a re-login, a foreign
    /// capture) or none (a cleared login): nothing was written and the stage
    /// is gone. `store` is the store's login as read.
    Refused { store: Option<ClaudeCredentials> },
}

/// Land a pair in the profile's store and run the post-persist hooks — the one
/// config-free writer for a pair a rotation minted. Takes the state flock
/// itself; the caller holds the profile's [`RotationGuard`] and no state-flock
/// hold of its own (the Keychain mirror runs after the hold ends).
///
/// The store's refresh token `D` is read raw FIRST in the hold. The pair is
/// written only when `D` is this chain's: `fp(D) = sent_fp` (the token this
/// pair was minted from), or `D` is spent, or `D` is the base of the stage on
/// disk (a rotation that sent a stage's token, or an evicted base). The stage
/// is read for its `base` alone and never vouches for a login the store does
/// not hold. Anything else refuses: an absent `D`, or a foreign login.
///
/// `in_hold` runs inside the same hold with the resolution, so a caller's
/// in-memory copy moves with the disk: `Some` for a landing or a refusal,
/// `None` when the store could not be read or written (the stage then stays
/// for a retry). `old_access` is the access token of the login the pair
/// replaces, which the hooks recognise in the live slot and Keychain.
pub(crate) fn land_pair(
    name: &ProfileName,
    sent_fp: &str,
    creds: &ClaudeCredentials,
    old_access: &str,
    in_hold: impl FnOnce(Option<&Landing>, &StateLockHeld),
) -> Result<Landing> {
    land_pair_with(
        name,
        LandMode::Write { sent_fp },
        creds,
        old_access,
        in_hold,
    )
}

/// Run [`land_pair`]'s post-persist hooks for a staged pair the store already
/// holds (a persist that died between its store write and its hooks), then
/// delete the stage: nothing is written to the store. A store that moved off
/// `creds` by the time the hold is taken refuses, and the stage goes.
fn land_hooks(name: &ProfileName, creds: &ClaudeCredentials, old_access: &str) -> Result<Landing> {
    land_pair_with(name, LandMode::HooksOnly, creds, old_access, |_, _| {})
}

/// Whether [`land_pair_with`] writes the store or only runs the hooks for a
/// pair the store already holds.
#[derive(Clone, Copy)]
enum LandMode<'a> {
    Write { sent_fp: &'a str },
    HooksOnly,
}

fn land_pair_with(
    name: &ProfileName,
    mode: LandMode<'_>,
    creds: &ClaudeCredentials,
    old_access: &str,
    in_hold: impl FnOnce(Option<&Landing>, &StateLockHeld),
) -> Result<Landing> {
    // Rotation coherence (#1): a rotation of the ACTIVE profile revokes the
    // single-use refresh token the macOS Keychain copy carries — the running
    // `claude` (which re-reads the Keychain per request) would sign out at
    // that stale token's expiry while every clauth copy stays green (observed
    // on-device 2026-07-07). The mirror DECISION and the creds snapshot are
    // made under the locked section below, so the written pair is exactly the
    // persisted one; the `/usr/bin/security` shell-outs themselves run after
    // the flock is released (the global state flock must never be held across
    // a subprocess — before this function the locked section contained only
    // fast disk writes), all of them under one `runtime::KEYCHAIN_MIRROR_BUDGET`
    // armed once the hold ends.
    #[cfg(target_os = "macos")]
    let mut mirror: Option<ClaudeCredentials> = None;
    // The split-path mirror is decided under the lock but GATED after it: the
    // gate reads the Keychain item, a `security` subprocess the flock must
    // never span. `split_gate_prev` carries the recognition candidate out of
    // the locked section (a fast disk read inside it, the same class the
    // section already performs); both gates below also read `old_access`.
    #[cfg(target_os = "macos")]
    let mut split_mirror: Option<ClaudeCredentials> = None;
    #[cfg(target_os = "macos")]
    let mut split_gate_prev: Option<String> = None;
    // #104: entry-time install-source bearer, captured BEFORE the rotation
    // persists — what the file-layer follower's changed-check and recognition
    // candidates read the pre-rotation content against. Stable: every caller
    // holds this profile's RotationGuard across the function.
    let pre_hook_install_access = crate::claude::install_source_path(name)
        .ok()
        .and_then(|p| crate::profile::read_json_file::<ClaudeCredentials>(&p).ok())
        .and_then(|c| {
            c.access_token()
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        });
    // A hooks-only landing finds the pair already in the install source, so
    // its pre-landing content is the login it replaced: `old_access`.
    let pre_hook_install_access = match mode {
        LandMode::HooksOnly if pre_hook_install_access.as_deref() == creds.access_token() => {
            Some(old_access.to_string())
        }
        _ => pre_hook_install_access,
    };
    let landing = with_state_lock(|held| {
        let store = match read_raw_store(name) {
            Ok(store) => store,
            Err(e) => {
                in_hold(None, held);
                return Err(e.context("failed to persist rotated tokens"));
            }
        };
        let store_refresh = store.as_ref().and_then(ClaudeCredentials::refresh_token);
        let owned = match mode {
            LandMode::Write { sent_fp } => store_refresh.is_some_and(|d| {
                let d_fp = token_fingerprint(d);
                d_fp == sent_fp
                    || SpentRecord::read(name).spent(d) == Some(true)
                    || StagedPair::read(name).is_some_and(|g| g.base == d_fp)
            }),
            LandMode::HooksOnly => {
                store_refresh.is_some() && store_refresh == creds.refresh_token()
            }
        };
        if !owned {
            delete_stage(name);
            if matches!(mode, LandMode::Write { .. }) {
                logline!(
                    "clauth: '{name}': the store holds a different login now; kept it and \
                     dropped the refreshed pair"
                );
            }
            let landing = Landing::Refused { store };
            in_hold(Some(&landing), held);
            return Ok(landing);
        }
        // The profile may have been deleted or renamed out-of-process since the
        // caller's config was loaded (the single-fetcher holds a stale config
        // between reloads), and the store write below would recreate its
        // directory, so ask the on-disk list — under the flock, the one stable
        // answer — before it.
        if !crate::profile::is_configured(name).unwrap_or(false) {
            in_hold(None, held);
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        }
        if matches!(mode, LandMode::Write { .. })
            && let Err(e) = crate::profile::write_profile_credentials(name, Some(creds), held)
        {
            in_hold(None, held);
            return Err(e.context("failed to persist rotated tokens"));
        }
        delete_stage(name);
        // A live file Claude Code wrote beside the login just replaced (a
        // `/design-login`) no longer matches the store's login, so no later
        // sync would recognise it as this account's: saved now, matched
        // against the replaced token, once the stage above is resolved.
        crate::claude::sync_live_extra_after_rotation(name, old_access);
        // CLA-ROLL: the flag is read from DISK, never a caller's memory, which
        // can predate a completed `static-token --clear` in another process;
        // stamping from that would re-create the sidecar the operator was just
        // told is gone. `None` (an unreadable profile) stamps nothing and keeps
        // the rotating pair off the live slot and the Keychain.
        let rolling = crate::profile::load_profile(name)
            .ok()
            .map(|p| p.rolling_token);
        let is_active = crate::profile::active_profile_name().as_ref() == Some(name);
        // CLA-ROLL: a rolling-token split profile re-stamps its session token
        // from the freshly rotated chain on EVERY rotation, active or parked —
        // a fast disk write inside the locked section, same durability class
        // as the credential write above. The pair itself still never leaves
        // clauth custody; only the (refresh-less) access token rolls forward.
        // An ABSENT sidecar is stamped too (it arms on the next rotation —
        // closes the race where a switch gate sees a comfortable chain before
        // any sidecar exists); only a NotLongLived mis-fill is left alone, so
        // the roll never destroys evidence of whatever wrote it.
        // CLA-ROLL: the sidecar's PRE-stamp bearer, captured before the
        // re-stamp below overwrites it — what the macOS split mirror's
        // foreign gate recognizes the Keychain item against. A rolling bearer
        // changes on every stamp, so recognition needs the token being
        // REPLACED, never the one being written. When no sidecar exists yet
        // (the arming rotation), this falls through to `credentials.json`,
        // which by now holds the freshly rotated pair — harmless as a
        // candidate (the item cannot already hold a token this rotation has
        // not mirrored), and the pre-rotation chain token below covers the
        // login the vanilla mirror actually wrote.
        #[cfg(target_os = "macos")]
        {
            split_gate_prev = crate::claude::install_source_path(name)
                .ok()
                .and_then(|p| crate::profile::read_json_file::<ClaudeCredentials>(&p).ok())
                .and_then(|c| {
                    c.access_token()
                        .filter(|t| !t.is_empty())
                        .map(str::to_string)
                });
        }
        let stamp_sidecar = rolling == Some(true)
            && !matches!(
                crate::claude::session_token_status(name),
                Some(crate::claude::SessionTokenStatus::NotLongLived)
            );
        if stamp_sidecar
            && let Some(oauth) = creds.claude_ai_oauth.as_ref()
            && let Err(e) = crate::claude::stamp_rolling_token(name, oauth)
        {
            // Loud, non-fatal: the rotation is durable; the next rotation or
            // the switch-in gate retries the stamp. The stale rolling token keeps
            // serving until its real expiry, and every surface shows that
            // countdown honestly.
            logline!("clauth: rotated '{name}' but re-stamping session-token.json failed: {e:#}");
        }
        // Read AFTER the stamp above: the arming rotation's stamp is what
        // creates the sidecar this asks about.
        let rolling_unsplit =
            rolling != Some(false) && crate::claude::session_token_status(name).is_none();
        // #104: the file-layer live-slot follower, every OS — the live FILE is
        // what Linux sessions read, and a regular file CC wrote must follow
        // the rotation or it keeps serving the revoked login. Inside the
        // flock, so the follower's read-modify-write serializes with link
        // publishes. The rolling belt mirrors the macOS arm below: a
        // rolling-flagged profile whose stamp failed has NO sidecar, and a
        // rotating pair must never reach the live slot. A static mint's
        // steady state is quiet (the unchanged-check reads it as nothing to
        // follow).
        if is_active && !rolling_unsplit {
            let incoming = match crate::claude::install_source_path(name)
                .and_then(|path| crate::profile::read_json_file::<ClaudeCredentials>(&path))
            {
                Ok(creds) => Some(creds),
                Err(e) => {
                    logline!(
                        "clauth: rotated '{name}' but re-reading the install source failed: \
                         {e:#}. Live file left untouched; run `clauth {name}` to reinstall"
                    );
                    None
                }
            };
            if let Some(incoming) = incoming
                && let Some(oauth) = incoming.claude_ai_oauth.as_ref()
                && pre_hook_install_access.as_deref() != Some(oauth.access_token.as_str())
            {
                let candidates: Vec<&str> = [
                    pre_hook_install_access.as_deref(),
                    Some(old_access),
                    Some(oauth.access_token.as_str()),
                ]
                .into_iter()
                .flatten()
                .collect();
                match crate::claude::follow_live_file_login(&candidates, oauth) {
                    Ok(crate::claude::LiveSlotFollow::NotOurs) => logline!(
                        "clauth: rotated '{name}' but the live .credentials.json login is not \
                         one clauth recognizes (an out-of-band re-login, or a follow write \
                         that failed a rotation back). Live file left untouched; {}",
                        crate::format::RESOLVE_IN_TUI
                    ),
                    Ok(crate::claude::LiveSlotFollow::Unreadable(e)) => logline!(
                        "clauth: rotated '{name}' but the live .credentials.json could not be \
                         parsed to check its login ({e}); live file left untouched. Run \
                         `clauth {name}` to reinstall"
                    ),
                    Err(e) => logline!(
                        "clauth: rotated '{name}' but following the live .credentials.json \
                         failed: {e:#}. A running claude signs out when its old token expires; \
                         run `clauth {name}` to reinstall"
                    ),
                    Ok(_) => {}
                }
            }
        }
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() && is_active {
            if crate::claude::has_session_token(name) {
                // CLA-SPLIT: the live slot intentionally holds this profile's
                // static session token — the rotated pair is the clauth-private
                // USAGE chain and must never be mirrored over it. Quiet: this
                // is the designed steady state, not a divergence.
                // CLA-ROLL: what DOES ship to the Keychain for a rolling-token
                // profile is the freshly STAMPED sidecar (refresh-less bearer) —
                // the running claude re-reads the Keychain per request, so
                // this is exactly how the new token reaches live sessions.
                // The refresh-none re-check is a content-level belt: whatever
                // reaches the Keychain through the rolling path can never carry
                // a refresh token (invariant #1 — a rotating pair in front of
                // sessions is the death the split exists to prevent).
                if stamp_sidecar
                    && let Ok(path) = crate::claude::install_source_path(name)
                    && let Ok(sidecar) = crate::profile::read_json_file::<ClaudeCredentials>(&path)
                    && sidecar.refresh_token().is_none()
                {
                    // CLA-SPLIT: the mirror is only DECIDED here; the foreign
                    // gate that guards `Keep::Everything` runs AFTER the lock
                    // closure (it reads the Keychain item, a subprocess the
                    // flock must never span — same discipline as the write
                    // itself). See the gate block below `with_state_lock`.
                    split_mirror = Some(sidecar);
                }
            } else if live_login_is_foreign(name, old_access) {
                logline!(
                    "clauth: rotated '{name}' but the live login diverged (a re-login clauth \
                     doesn't own). Keychain left untouched; {}",
                    crate::format::RESOLVE_IN_TUI
                );
            } else if rolling_unsplit {
                // CLA-ROLL: flag on but NO sidecar right now — the arming stamp
                // write just failed (logged above). Never ship the rotating
                // pair to the Keychain for a rolling-token profile; the
                // previous rolling bearer keeps serving until the roll heals
                // (next rotation, the switch gate, or a `clauth rolling-token` re-arm).
                // A NotLongLived mis-fill deliberately does NOT take this
                // branch: a disengaged split behaves as vanilla (the pair
                // mirror below is what keeps CC alive there).
            } else {
                mirror = Some(creds.clone());
            }
        }
        in_hold(Some(&Landing::Landed), held);
        Ok(Landing::Landed)
    })?;
    // Armed past the hold, so the flock wait above (and a caller's token call
    // before it) spends none of the window the Keychain calls below share.
    crate::lock::with_keychain_budget(crate::runtime::KEYCHAIN_MIRROR_BUDGET, || {
        // A failed state flock surfaces as the `Err` from `with_state_lock` above,
        // so a poisoned/unavailable lock never looks like a successful rotation.
        // A mirror failure is loud but non-fatal: the rotation itself is durable,
        // and the next rotation or switch retries the write.
        #[cfg(target_os = "macos")]
        if let Some(creds) = mirror {
            // The vanilla mirror writes `Keep::Everything` too, so the item's
            // login must be one clauth put there first — the same foreign gate
            // the split mirror runs (the reasons sit on its block below).
            // Candidates this path knows: the pre-rotation bearer (what an
            // earlier mirror wrote) and the bearer being written (the idempotent
            // re-mirror).
            let candidates: Vec<&str> = [Some(old_access), creds.access_token()]
                .into_iter()
                .flatten()
                .collect();
            match crate::keychain::item_login_state(&candidates) {
                crate::keychain::ItemLoginState::Ours
                | crate::keychain::ItemLoginState::Corrupt => {
                    if let Err(e) = crate::keychain::keychain_mirror_rotation(&creds) {
                        logline!(
                            "clauth: rotated '{name}' but the Keychain mirror failed: {e:#}. A \
                         running claude signs out when its old token expires; run `clauth {name}` \
                         to reinstall"
                        );
                    }
                }
                crate::keychain::ItemLoginState::NotOurs => logline!(
                    "clauth: rotated '{name}' but the macOS Keychain login is not one clauth \
                 recognizes (an out-of-band re-login, or a mirror write that failed a rotation \
                 back). Keychain left untouched; {}",
                    crate::format::RESOLVE_IN_TUI
                ),
                crate::keychain::ItemLoginState::Unreadable(e) => logline!(
                    "clauth: rotated '{name}' but the macOS Keychain item could not be read to \
                 check its login ({e}); mirror skipped, the previous bearer keeps serving until \
                 it expires. Run `clauth {name}` to reinstall"
                ),
            }
        }
        // CLA-SPLIT foreign gate for the rolling mirror: `Keep::Everything`
        // preserves the item's sibling blocks, so the item's login must be one
        // clauth put there first — the file layer stops being evidence once CC
        // migrates into the Keychain, and an out-of-band `/login` leaves B's
        // blocks to ride under A's bearer. Runs here, beside the write it gates,
        // because the read is a `security` subprocess the state flock must never
        // span. Candidates: the sidecar's pre-stamp bearer, the pre-rotation
        // chain token, and the bearer being written (the item may already hold
        // it — the idempotent re-mirror).
        #[cfg(target_os = "macos")]
        if let Some(creds) = split_mirror {
            let candidates: Vec<&str> = [
                split_gate_prev.as_deref(),
                Some(old_access),
                creds.access_token(),
            ]
            .into_iter()
            .flatten()
            .collect();
            match crate::keychain::item_login_state(&candidates) {
                // Corrupt proceeds with the write: the mirror's own read leg
                // quarantines the truncated bytes and the write heals the item,
                // the pre-gate behavior for that state.
                crate::keychain::ItemLoginState::Ours
                | crate::keychain::ItemLoginState::Corrupt => {
                    if let Err(e) = crate::keychain::keychain_mirror_rotation(&creds) {
                        logline!(
                            "clauth: rotated '{name}' but the Keychain mirror failed: {e:#}. A \
                         running claude signs out when its old token expires; run `clauth {name}` \
                         to reinstall"
                        );
                    }
                }
                crate::keychain::ItemLoginState::NotOurs => logline!(
                    "clauth: rotated '{name}' but the macOS Keychain login is not one clauth \
                 recognizes (an out-of-band re-login, or a mirror write that failed a rotation \
                 back). Keychain left untouched; {}",
                    crate::format::RESOLVE_IN_TUI
                ),
                crate::keychain::ItemLoginState::Unreadable(e) => logline!(
                    "clauth: rotated '{name}' but the macOS Keychain item could not be read to \
                 check its login ({e}); mirror skipped, the previous rolling bearer keeps \
                 serving until it expires. Run `clauth {name}` to reinstall"
                ),
            }
        }
        Ok(landing)
    })
}

/// Why a rotation's persist did not land its pair.
pub(crate) enum PersistError {
    /// The store holds a login this chain does not own, or none: nothing was
    /// written, the stage is gone, and the caller's in-memory profile now
    /// holds `store`. A caller carrying a pair back to a token list carries
    /// the store's ([`Self::carried_pair`]), never the minted one.
    Refused {
        store: Option<Box<ClaudeCredentials>>,
    },
    /// The flock, the store read or the write failed: the stage stays for a
    /// retry, and the minted pair is still the chain head.
    Failed(anyhow::Error),
}

impl PersistError {
    /// A refusal's `(access, refresh)` pair from the store, `None` when the
    /// store holds no login (the caller drops its token-list entry).
    pub(crate) fn carried_pair(
        store: Option<&ClaudeCredentials>,
    ) -> Option<(String, Option<String>)> {
        let oauth = store?.claude_ai_oauth.as_ref()?;
        Some((oauth.access_token.clone(), oauth.refresh_token.clone()))
    }

    /// The `OpResult` / log rendering: every persist-side outcome reads as the
    /// one "failed to persist rotated tokens" sentence, a refusal included.
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Failed(e) => e,
            Self::Refused { .. } => anyhow::anyhow!("failed to persist rotated tokens"),
        }
    }
}

/// The rotation persist: land `creds` (the post-answer step's [`Minted`]
/// block) through [`land_pair`] with `sent_fp` and `old_access`, and move the
/// in-memory profile in the same hold — to the landed pair, to the store's
/// login on a refusal, or to the minted pair when the write failed (it is the
/// chain head either way). `config.toml` is never written: a rotation changes
/// nothing there, and a whole-profile save from a stale in-memory profile
/// reverted other processes' edits. Takes `&ConfigHandle` so workers call it
/// without holding the lock across HTTP.
pub(crate) fn apply_rotated_tokens_locked(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    sent_fp: &str,
    creds: &ClaudeCredentials,
    old_access: &str,
) -> std::result::Result<(), PersistError> {
    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    let mut cfg = config.lock().expect("config mutex poisoned");
    if cfg.find(name).is_none() {
        return Err(PersistError::Failed(anyhow::anyhow!(
            "failed to persist rotated tokens"
        )));
    }
    let landing = land_pair(name, sent_fp, creds, old_access, |landing, held| {
        let Some(profile) = cfg.find_mut(name) else {
            return;
        };
        let next = match landing {
            Some(Landing::Refused { store }) => store.clone(),
            Some(Landing::Landed) | None => Some(creds.clone()),
        };
        profile.set_credentials(next, held);
    })
    .map_err(PersistError::Failed)?;
    match landing {
        Landing::Landed => Ok(()),
        Landing::Refused { store } => Err(PersistError::Refused {
            store: store.map(Box::new),
        }),
    }
}

/// The plight-latch key for an unreadable spent record.
const SPENT_RECORD_PLIGHT: &str = "spent-record";

/// What the send-site rule lets a spend site do with the refresh token `rt` it
/// read: never send a spent or superseded token.
pub(crate) enum SendRule {
    /// Send this token: the live stage's (the chain head) or `rt`.
    Send(String),
    /// `rt` is spent and the store holds a different, unspent login: adopt it
    /// into memory ([`adopt_disk_rotation`]) and stop the leg without sending.
    /// Carries the store's `(access, refresh)` pair for a caller's token list.
    Adopt((String, Option<String>)),
    /// `rt` is spent and nothing on disk supersedes it: quarantine, no send.
    Quarantine,
    /// The spent record cannot be read (or, under a spent `rt`, the store):
    /// skip this spend; a later tick retries.
    Skip,
}

/// Decide what a spend site holding `name`'s rotation guard may send, after
/// reading `rt`. In order: an unreadable record skips (every later rule reads
/// it); a live stage sends its token whether or not `rt` is spent; a spent `rt`
/// reads the store, adopting a different unspent login and quarantining
/// otherwise; anything else sends `rt`.
pub(crate) fn send_rule(name: &ProfileName, rt: &str, _guard: &RotationGuard) -> SendRule {
    let record = SpentRecord::read(name);
    if matches!(record, SpentRecord::Unreadable) {
        if crate::codex_auth::plight_warn_once(name.as_str(), SPENT_RECORD_PLIGHT) {
            logline!(
                "clauth: '{name}': cannot read its spent-token record; token refreshes wait \
                 until it reads again"
            );
        }
        return SendRule::Skip;
    }
    crate::codex_auth::clear_plight_warn(name.as_str(), SPENT_RECORD_PLIGHT);
    let store = read_raw_store(name);
    let d = store
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .and_then(ClaudeCredentials::refresh_token);
    if store.is_ok()
        && let Some(stage) = StagedPair::read(name)
        && stage.state(d, &record) == StageState::Live
        && let Some(head) = stage.creds.refresh_token()
    {
        return SendRule::Send(head.to_string());
    }
    if record.spent(rt) != Some(true) {
        return SendRule::Send(rt.to_string());
    }
    match &store {
        Err(_) => SendRule::Skip,
        Ok(Some(store)) if d.is_some_and(|d| d != rt && record.spent(d) == Some(false)) => {
            SendRule::Adopt((
                store.access_token().unwrap_or_default().to_string(),
                d.map(str::to_string),
            ))
        }
        Ok(_) => SendRule::Quarantine,
    }
}

/// The `Transient` a skipped spend or a refused landing reports when the spent
/// record cannot be read: a filesystem fault under `~/.clauth`.
pub(crate) fn spent_record_unreadable(name: &ProfileName) -> crate::format::Transient {
    crate::format::Transient::new(
        crate::format::Cause::StateLockUnavailable(name.to_string()),
        crate::format::Retry::Stated,
    )
}

/// What a guard holder found staged for a profile and did with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageOutcome {
    /// No stage on disk.
    Absent,
    /// The live stage's pair landed in the store.
    Landed,
    /// The store holds a login the stage's chain does not own; the stage is
    /// gone and the store kept.
    Refused,
    /// The store already held the staged pair; its hooks ran, the stage is gone.
    HooksRan,
    /// The stage was inert and is gone.
    Dropped,
    /// The store still holds the stage's base or pair but the spent record
    /// cannot be read, so nothing can judge it: left as it is.
    Unjudgeable,
}

/// How a stage reader judges `stage` against the store's refresh token `d` and
/// the record: its state, or `None` when the store is still bound to it
/// (holding its base or its pair) and the record cannot be read, so nothing
/// may judge it.
fn judge_stage(stage: &StagedPair, d: Option<&str>, record: &SpentRecord) -> Option<StageState> {
    let bound = d.is_some_and(|d| {
        token_fingerprint(d) == stage.base || Some(d) == stage.creds.refresh_token()
    });
    if bound && matches!(record, SpentRecord::Unreadable) {
        return None;
    }
    Some(stage.state(d, record))
}

/// Read lock-free whether `name`'s stage needs a guard holder before its store
/// is installed: false for an absent stage and for one judged inert (which
/// lands nothing and which the catch-up deletes), true for every other one,
/// an unreadable store included, so the lander decides it under the guard.
fn stage_needs_landing(name: &ProfileName) -> bool {
    let Some(stage) = StagedPair::read(name) else {
        return false;
    };
    let Ok(store) = read_raw_store(name) else {
        return true;
    };
    let d = store.as_ref().and_then(ClaudeCredentials::refresh_token);
    judge_stage(&stage, d, &SpentRecord::read(name)) != Some(StageState::Inert)
}

/// Resolve `name`'s stage under its rotation guard by its three states: live
/// lands through `land_live`, landed runs the hooks with the base pair as the
/// old one, inert is deleted. A stage the store is still bound to (holding its
/// base or its pair) is never judged off an unreadable record.
fn resolve_stage(
    name: &ProfileName,
    _guard: &RotationGuard,
    land_live: impl FnOnce(&StagedPair) -> Result<Landing>,
) -> Result<StageOutcome> {
    let Some(stage) = StagedPair::read(name) else {
        return Ok(StageOutcome::Absent);
    };
    let store = read_raw_store(name)?;
    let d = store.as_ref().and_then(ClaudeCredentials::refresh_token);
    let Some(state) = judge_stage(&stage, d, &SpentRecord::read(name)) else {
        return Ok(StageOutcome::Unjudgeable);
    };
    Ok(match state {
        StageState::Live => match land_live(&stage)? {
            Landing::Landed => StageOutcome::Landed,
            Landing::Refused { .. } => StageOutcome::Refused,
        },
        StageState::Landed => match land_hooks(name, &stage.creds, &stage.base_access)? {
            Landing::Landed => StageOutcome::HooksRan,
            Landing::Refused { .. } => StageOutcome::Dropped,
        },
        StageState::Inert => {
            delete_stage(name);
            StageOutcome::Dropped
        }
    })
}

/// Land `name`'s stage under its rotation guard, then move the in-memory profile
/// with it (the landed pair, or the store's login on a refusal), lifting a
/// standing `auth_broken` once a live stage lands: that chain is alive. The
/// catch-up leg's body and the install gate's lander.
///
/// `config` is NOT held across the landing, whose state-flock wait can run to
/// its deadline behind a peer: the catch-up runs on a TUI lease holder's tick
/// too, whose UI needs the config meanwhile. Memory moves in a second, short
/// hold after the landing; a memory move that cannot take the flock is logged
/// and leaves memory on its old pair, which a later spend judges against the
/// store under its own guard (`send_rule`, the persist's ownership check).
pub(crate) fn land_staged_pair(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    guard: &RotationGuard,
) -> Result<StageOutcome> {
    let mut next: Option<Option<ClaudeCredentials>> = None;
    let outcome = resolve_stage(name, guard, |stage| {
        let landing = land_pair(
            name,
            &stage.base,
            &stage.creds,
            &stage.base_access,
            |_, _| {},
        )?;
        next = Some(match &landing {
            Landing::Landed => Some(stage.creds.clone()),
            Landing::Refused { store } => store.clone(),
        });
        Ok(landing)
    })?;
    if let Some(next) = next
        && let Ok(mut cfg) = config.lock()
        && let Some(profile) = cfg.find_mut(name)
        && let Err(e) = with_state_lock(|held| {
            profile.set_credentials(next, held);
            Ok(())
        })
    {
        logline!(
            "clauth: '{name}': saved the waiting token pair, but this process still holds the \
             old one: {e:#}"
        );
    }
    if outcome == StageOutcome::Landed && config.lock().is_ok_and(|cfg| cfg.is_auth_broken(name)) {
        mark_auth_broken(config, name, false);
    }
    Ok(outcome)
}

/// The lander a guard holder runs before it copies `name`'s store anywhere a
/// Claude Code reads, or returns it as installable: a live stage lands first,
/// with the guard held and before any state-flock hold of the caller's own. A
/// landing that cannot complete refuses the caller's operation as `Transient`
/// (the flock busy or faulted, the store or record unreadable); the caller
/// then never installs or seeds from the store.
pub(crate) fn land_stage_for_install(
    name: &ProfileName,
    guard: &RotationGuard,
) -> std::result::Result<(), crate::format::Transient> {
    match resolve_stage(name, guard, |stage| {
        land_pair(
            name,
            &stage.base,
            &stage.creds,
            &stage.base_access,
            |_, _| {},
        )
    }) {
        Ok(StageOutcome::Unjudgeable) => Err(spent_record_unreadable(name)),
        Ok(_) => Ok(()),
        Err(e) => Err(adopt_lock_transient(name, &e)),
    }
}

/// [`land_stage_for_install`] for a caller holding a config handle: the
/// in-memory profile follows the landing ([`land_staged_pair`]).
fn land_stage_for_gate(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    guard: &RotationGuard,
) -> std::result::Result<(), crate::format::Transient> {
    match land_staged_pair(config, name, guard) {
        Ok(StageOutcome::Unjudgeable) => Err(spent_record_unreadable(name)),
        Ok(_) => Ok(()),
        Err(e) => Err(adopt_lock_transient(name, &e)),
    }
}

/// Whether `name` has a stage file on disk; no directory is created.
pub(crate) fn has_stage(name: &ProfileName) -> bool {
    crate::profile::profile_subpath(name, STAGE_FILE).is_ok_and(|p| p.is_file())
}

/// What the spent record beside a store says about copying a session-side login
/// over that store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLoginCopy {
    /// Unspent, or carrying no refresh token (which cannot be spent): copy.
    Copyable,
    /// The server already consumed its refresh token: copying it back would
    /// rewind the chain onto a dead token, so the store wins.
    Spent,
    /// The record cannot be read, so the login may be the chain head or a dead
    /// token: neither side may be written over the other.
    Unknown,
}

/// Judge a session-side login against the spent record beside `store`.
pub(crate) fn session_login_copy(
    store: &std::path::Path,
    login: &ClaudeCredentials,
) -> SessionLoginCopy {
    let Some(rt) = login.refresh_token() else {
        return SessionLoginCopy::Copyable;
    };
    match SpentRecord::beside(store).spent(rt) {
        Some(false) => SessionLoginCopy::Copyable,
        Some(true) => SessionLoginCopy::Spent,
        None => SessionLoginCopy::Unknown,
    }
}

/// Adopt the live session's OWN token rotation instead of fighting it
/// (rotation coherence — the future-proof half). The running `claude` and
/// clauth hold ONE single-use refresh family; whoever refreshes first revokes
/// the other. Rather than racing, concede: CC maintains
/// `~/.claude/.credentials.json` as a regular-file mirror of its Keychain
/// login (rewritten at least on every CC launch), a prompt-free read path to
/// CC's current pair. When that mirror holds a FRESHER pair for the SAME
/// account, adopt it into the profile store — no refresh spent — so clauth
/// stays correct whatever refresh schedule a future Claude Code ships.
///
/// Gates, in order — every one must pass:
///   * `name` is the ACTIVE profile (only its chain is shared with a live CC);
///   * the live path classifies [`crate::claude::LinkState::Diverged`]
///     (`LinkedTo` = mirror equals the store, nothing to adopt);
///   * the mirror pair carries a refresh token and a STRICTLY LATER expiry
///     than the store (never adopt sideways or backwards);
///   * identity: the mirror token's account uuid (via `identity`, injected so
///     the gate is testable offline; prod passes `usage::fetch_account_uuid`)
///     matches the profile's cached uuid — or, when no uuid is cached yet, the
///     STORED token's own uuid fetched now (only possible while it still
///     works). Unprovable identity refuses the adopt: a live login belonging
///     to a different account (a manual CC `/login`) must never be captured
///     into this profile unattended — that stays the TUI divergence flow's
///     job.
///
/// On success the mirror uuid is cached (`ACCOUNT_ID_CACHE_FILE`), so later
/// adopts can verify identity even when the stored token is already dead.
/// The Keychain is NOT written here — in this state CC minted the pair, so
/// the Keychain and mirror are already the fresh truth; only our store lags.
///
/// Returns the adopted `(access, refresh)` pair so the caller can sync its
/// in-memory `TokenList` exactly like every other rotation site — without it,
/// the next poll would run on the superseded entry, spend the revoked refresh
/// token, and fail on the very account the adopt just saved.
///
/// `_rotation_guard` is proof the caller holds this profile's per-profile
/// rotation lock: the adopt mutates the same stored credential fields as a
/// refresh persist (`rotate_one_inner`), so both writers must serialize on
/// the same [`crate::runtime::RotationGuard`], not just the state flock.
/// Taken by reference because the flock is not reentrant — the refresh-failure
/// call site already holds the guard when it retries the adopt.
///
/// The stored-token identity probe below is gated by a bounded negative cache.
/// The STORED token is static: once it fails to prove identity (revoked
/// upstream while still clock-valid), it will fail every leg, and re-probing
/// it each time spends a `/profile` against an account already in trouble. So
/// a failed stored-token probe is suppressed for a window. The suppression is
/// a TTL, never a permanent `None` (a transient failure becomes retryable once
/// it lapses), and it never applies to the LIVE mirror token — whose probe the
/// fresh-pair adopt depends on re-running within a leg.
const STORED_PROBE_SUPPRESS_TTL_MS: u64 = 15 * 60 * 1000;

/// Per-stored-token-hash → the earliest epoch-ms a `/profile` identity probe
/// may run again. Consulted only by [`try_adopt_live_rotation`]'s stored-token
/// arm; the live-mirror probe and the `Some`-only identity memo are untouched.
/// Keyed by the same SHA-256 [`crate::usage::identity_key`] the memo uses, so a
/// replaced stored token (a fresh hash after a successful refresh) is never
/// suppressed. Same leaf rank as the memo: the two maps are never held together.
static STORED_PROBE_SUPPRESSED: LazyLock<RankedMutex<HashMap<[u8; 32], u64>, rank::IdentityMemo>> =
    LazyLock::new(|| RankedMutex::new(HashMap::new()));

/// Whether the stored token may be identity-probed again, spending a `/profile`.
fn stored_probe_due(key: &[u8; 32]) -> bool {
    let now = now_ms();
    let Ok(suppressed) = STORED_PROBE_SUPPRESSED.lock() else {
        // A poisoned lock probes rather than silently refuses a legit adopt.
        return true;
    };
    suppressed
        .get(key)
        .is_none_or(|not_before| now >= *not_before)
}

fn suppress_stored_probe(key: &[u8; 32]) {
    if let Ok(mut suppressed) = STORED_PROBE_SUPPRESSED.lock() {
        suppressed.insert(*key, now_ms() + STORED_PROBE_SUPPRESS_TTL_MS);
    }
}

/// A stored token whose probe just succeeded has its positive answer memoized,
/// so the suppression entry is dead weight.
fn clear_stored_probe_suppression(key: &[u8; 32]) {
    if let Ok(mut suppressed) = STORED_PROBE_SUPPRESSED.lock() {
        suppressed.remove(key);
    }
}

#[cfg(test)]
pub(crate) fn reset_stored_probe_suppression() {
    if let Ok(mut suppressed) = STORED_PROBE_SUPPRESSED.lock() {
        suppressed.clear();
    }
}

#[cfg(test)]
fn set_stored_probe_not_before_for_test(key: &[u8; 32], not_before: u64) {
    if let Ok(mut suppressed) = STORED_PROBE_SUPPRESSED.lock() {
        suppressed.insert(*key, not_before);
    }
}

/// Dedupe keys recorded in [`crate::profile_cache::ADOPT_REFUSAL_FILE`]. Each
/// refusal extends its reason key with the live account id (a failed or blank
/// probe falls back to the bare reason), so a DIFFERENT live login under
/// either standing state is a state change worth a line while the same
/// login's token churn (CC rewriting the mirror on every launch) stays silent.
const REFUSAL_UNPROVABLE_IDENTITY: &str = "unprovable-identity";
const REFUSAL_FOREIGN_ACCOUNT: &str = "foreign-account";

/// Whether this refusal is news. The refusal is a standing state — the
/// classify gate keeps reading `Diverged` while the live slot stays
/// unadoptable, and the leg re-fires every poll — so announcing unconditionally
/// writes one identical line per leg per process and drowns the daemon and TUI
/// logs; a refusal that never re-announces hides a NEW state. The last
/// announced key is therefore recorded beside the profile's other caches
/// (in-memory would not cross the daemon/TUI process boundary): the same
/// record-what-you-return-true-for contract as `SessionSwap::should_announce`,
/// returning `true` only when the key differs from the recorded one. The
/// record is dropped the moment a leg observes the state resolved — the
/// classify gate reading healthy, an adopt landing, or the session-token
/// regime switch — so a later standing state announces again; only a
/// resolution that re-diverges between two legs to the SAME account goes
/// unseen, and with it the new state's first line.
fn adopt_refusal_should_announce(name: &ProfileName, key: &str) -> bool {
    if crate::profile_cache::load_profile_cache::<String>(
        name,
        crate::profile_cache::ADOPT_REFUSAL_FILE,
    )
    .as_deref()
        == Some(key)
    {
        return false;
    }
    crate::profile_cache::write_profile_cache(name, crate::profile_cache::ADOPT_REFUSAL_FILE, &key);
    true
}

pub(crate) fn try_adopt_live_rotation(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    _rotation_guard: &crate::runtime::RotationGuard,
    identity: &dyn Fn(&str) -> Option<AccountId>,
) -> Option<(String, Option<String>)> {
    use crate::profile_cache::{
        ACCOUNT_ID_CACHE_FILE, ADOPT_REFUSAL_FILE, load_profile_cache, remove_profile_cache,
        write_profile_cache,
    };

    // CLA-SPLIT: this profile's live slot holds its STATIC session token, so
    // `classify_credentials_link` judges it against `session-token.json` while
    // every gate below reads and every write targets the clauth-private usage
    // pair in `credentials.json`. A live slot that stops holding the static
    // token classifies Diverged, and adopting would overwrite the usage chain
    // with a login that is not it. Same invariant
    // `snapshot_active_credentials_unchecked` carries for the capture sinks.
    if crate::claude::has_session_token(name) {
        // The adopt refusals are unreachable behind this gate, so a record
        // from this profile's OAuth era must not outlive the regime switch:
        // cleared here, a later OAuth re-divergence announces fresh.
        remove_profile_cache(name, ADOPT_REFUSAL_FILE);
        return None;
    }

    // Snapshot the store side under the config lock, then drop it — the
    // identity fetches below are HTTP and must never hold the mutex.
    let (stored_access, stored_expires) = {
        let Ok(cfg) = config.lock() else { return None };
        if !cfg.is_active(name) {
            return None;
        }
        let p = cfg.find(name)?;
        (
            p.access_token().map(str::to_string),
            p.access_token_expires_at(),
        )
    };

    if !matches!(
        crate::claude::classify_credentials_link(name),
        Ok(crate::claude::LinkState::Diverged)
    ) {
        // Anything but a live `Diverged` reading drops the record: a healthy
        // slot, a missing one, and a classify that could not read at all.
        // Dropping on the unreadable cases is the safe direction, since
        // holding the record through one would suppress the NEXT genuine
        // refusal, where dropping costs at most one duplicate announcement per
        // blip. A future standing refusal, the same reason included, is then
        // news again.
        remove_profile_cache(name, ADOPT_REFUSAL_FILE);
        return None;
    }
    let Ok(Some(live)) = crate::claude::read_claude_credentials() else {
        return None;
    };
    let live_oauth = live.claude_ai_oauth.as_ref()?;
    let live_refresh = live_oauth.refresh_token.as_ref()?;
    let (Some(live_expires), Some(stored_expires)) = (live_oauth.expires_at, stored_expires) else {
        return None;
    };
    if live_expires <= stored_expires {
        return None;
    }
    // Expiry is compared against memory, which can lag a store another process
    // landed since: a live pair the server already consumed (or one the record
    // cannot vouch for) is a rewind of that store, never an advance. The
    // predicate the watchdog's copy-back gates use.
    if SpentRecord::read(name).spent(live_refresh) != Some(false) {
        return None;
    }

    // Identity anchor: cached uuid, else the stored token's own uuid while it
    // still authenticates. No anchor → refuse (identity unprovable).
    let expected: Option<AccountId> = load_profile_cache::<AccountId>(name, ACCOUNT_ID_CACHE_FILE)
        .or_else(|| {
            let alive = (now_ms() as i64) < stored_expires;
            match (&stored_access, alive) {
                (Some(tok), true) => {
                    let key = crate::usage::identity_key(tok);
                    if !stored_probe_due(&key) {
                        return None;
                    }
                    let r = identity(tok);
                    if r.is_some() {
                        clear_stored_probe_suppression(&key);
                    } else {
                        suppress_stored_probe(&key);
                    }
                    r
                }
                _ => None,
            }
        });
    let Some(expected) = expected else {
        // The live token is probed for the ANNOUNCEMENT key, not for the
        // verdict (none is needed — no expectation to compare). The live
        // mirror is the fresher working login, so its probe succeeds and
        // memoizes like the foreign arm's; a failed or blank probe falls back
        // to the bare reason key.
        let key = identity(&live_oauth.access_token)
            .filter(|id| !id.trim().is_empty())
            .map(|id| format!("{REFUSAL_UNPROVABLE_IDENTITY}:{id}"))
            .unwrap_or_else(|| REFUSAL_UNPROVABLE_IDENTITY.to_string());
        if adopt_refusal_should_announce(name, &key) {
            logline!(
                "clauth: live login for '{name}' is newer but its identity can't be proven \
                 (no cached account id and the stored token is dead). Not adopting; \
                 resolve in the clauth TUI or re-run clauth login {name}"
            );
        }
        return None;
    };
    let live_id = identity(&live_oauth.access_token)?;
    // A blank uuid is shape drift, not an identity — two blanks matching each
    // other must never prove two tokens are the same account.
    if live_id.trim().is_empty() || expected.trim().is_empty() {
        return None;
    }
    if live_id != expected {
        let refusal_key = format!("{REFUSAL_FOREIGN_ACCOUNT}:{live_id}");
        if adopt_refusal_should_announce(name, &refusal_key) {
            logline!(
                "clauth: live login for '{name}' belongs to a DIFFERENT account. Not adopting; \
                 capture it via the clauth TUI divergence flow if that was intentional"
            );
        }
        return None;
    }

    // Persist under config mutex + state flock, re-checking the gates that
    // could have moved during the HTTP window (an interleaved switch or a
    // rotation that already advanced the store past the mirror).
    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    let mut cfg = config.lock().expect("config mutex poisoned");
    let adopted = with_state_lock(|held| {
        if !cfg.is_active(name) {
            return Ok(false);
        }
        // Fresh, not just the in-memory active marker: the profile may have been
        // deleted or renamed after this caller loaded its config but before the
        // rotation guard was acquired (a delete/rename takes the same guard, so
        // it cannot land while this leg holds it). The store write would recreate
        // its directory, so consult the on-disk list before writing.
        if !crate::profile::is_configured(name).unwrap_or(false) {
            return Ok(false);
        }
        let Some(profile) = cfg.find_mut(name) else {
            return Ok(false);
        };
        if profile
            .access_token_expires_at()
            .is_none_or(|cur| live_expires <= cur)
        {
            return Ok(false);
        }
        profile.set_credentials(Some(live.clone()), held);
        // The login alone: the adopt changes nothing in `config.toml`, and a
        // whole-profile save from this long-lived config would revert another
        // process's settings edit.
        crate::profile::write_profile_credentials(name, Some(&live), held)?;
        // The adopted pair is provably `live_id`'s account (the identity gate
        // above compared the mirror probe against it), so the anchor rides the
        // SAME hold that committed the store: a cross-process install's removal
        // landing in between cannot be overwritten by a stale uuid.
        write_profile_cache(name, ACCOUNT_ID_CACHE_FILE, &live_id);
        Ok::<bool, anyhow::Error>(true)
    })
    .unwrap_or(false);
    if !adopted {
        return None;
    }
    // This adopt IS the resolution of the divergence the refusal announced:
    // drop the once-per-state record so a future standing refusal — same
    // reason included — is news again.
    remove_profile_cache(name, ADOPT_REFUSAL_FILE);
    // The adopted pair proves the chain is alive, so a standing `auth_broken`
    // is stale — the flag was set while CC held the fresher pair. Same lift as
    // the scheduler's `carry_external_rotation` (inlined here because the
    // config guard is already held); without it, an active recovered by a
    // CC-side re-login stays excluded from the fallback walk and refused as a
    // switch target until a manual `clauth login`.
    if cfg.set_auth_broken(name, false) {
        logline!("clauth: '{name}' re-authenticated: auth_broken cleared");
        // Persist against fresh disk state (see `set_auth_broken_persisted`):
        // the adopt just proved the chain is alive, but this process's config
        // may be older than a concurrent CLI account mutation.
        let _ = crate::profile::set_auth_broken_persisted(name, false);
    }
    logline!(
        "clauth: adopted the live session's rotated login for '{name}' \
         (the running claude refreshed first, so no token spent)"
    );
    // Off macOS the hand-back path is the SYMLINK, and CC's refresh renames a
    // temp sibling over the live slot — `rename(2)` acts on the link, so the
    // divergence this adopt just resolved is the same event that destroyed it.
    // Restore it or the live slot stays a regular file that now classifies
    // LinkedTo (the tokens match), nothing relinks it, and our NEXT rotation
    // writes the store alone: the running claude signs out at the token we
    // just adopted. Content-neutral here — store and live hold the same pair.
    // macOS is excluded on purpose: CC reads the Keychain there and already
    // holds the pair it minted, so this would only issue a Keychain mirror over
    // an item that already matches. Loud but non-fatal, like the rotation mirror:
    // the adopted pair is already persisted, and dropping it would strand the
    // caller's TokenList on the refresh token CC revoked.
    #[cfg(not(target_os = "macos"))]
    if let Err(e) = crate::claude::force_link_profile_credentials(name) {
        logline!(
            "clauth: adopted the live login for '{name}' but relinking \
             .credentials.json failed: {e:#}. A running claude signs out when \
             its token expires; run `clauth {name}` to reinstall"
        );
    }
    Some((
        live_oauth.access_token.clone(),
        live_oauth.refresh_token.clone(),
    ))
}

/// Whether the live `.credentials.json` holds a login clauth does NOT own —
/// i.e. genuinely [`crate::claude::LinkState::Diverged`] and not merely a
/// stale regular-file mirror of this profile's own pre-rotation pair. On
/// macOS Claude Code rewrites the live file as a regular-file copy of the
/// Keychain, so the moment a rotation lands, `classify_credentials_link`
/// reports Diverged against the NEW stored token even though the live login
/// is still our own chain one step behind — that stale-mirror case must still
/// be mirrored, or the coherence write would skip exactly when it matters.
/// Only a live token matching NEITHER the new nor the pre-rotation pair is
/// foreign (a real CC re-login); an unreadable/unclassifiable state is
/// treated as foreign so a state we cannot understand is never overwritten.
#[cfg(target_os = "macos")]
fn live_login_is_foreign(name: &ProfileName, old_access: &str) -> bool {
    match crate::claude::classify_credentials_link(name) {
        Ok(crate::claude::LinkState::LinkedTo) | Ok(crate::claude::LinkState::Missing) => false,
        Ok(crate::claude::LinkState::Diverged) => {
            let live = crate::claude::read_claude_credentials().ok().flatten();
            let live_token = live.as_ref().and_then(|c| c.access_token());
            !live_token.is_some_and(|t| !t.is_empty() && t == old_access)
        }
        Err(_) => true,
    }
}

/// True when an active profile is set and its live .credentials.json no longer
/// resolves to that profile's stored credentials. Then the in-memory tokens are
/// stale relative to what CC just wrote, so rotating them would leak a refresh
/// chain nobody will use.
fn active_link_diverged(config: &AppConfig) -> bool {
    config.state.active_profile.as_ref().is_some_and(|name| {
        matches!(
            classify_credentials_link(name).ok(),
            Some(LinkState::Diverged)
        )
    })
}

/// Grace window (ms): a token with less than this much life left is treated as
/// expiring, so the AUTH-1 gate refreshes it *before* install rather than
/// letting the freshly-switched session hit a 401. The bound is Claude Code's
/// own refresh threshold — CC starts refreshing a credential inside five
/// minutes of expiry, so anything installed with less life lands in a client
/// already trying to refresh it — and it is the SAME number the
/// backup-restore verdicts read ([`crate::claude::BACKUP_EXPIRY_GRACE_MS`],
/// the one home), so identical bytes can never read as dead in the backup
/// slot and installable in the live one.
const AUTH_GATE_GRACE_MS: i64 = crate::claude::BACKUP_EXPIRY_GRACE_MS;

/// Outcome of the pre-install auth gate ([`ensure_installable`]).
pub(crate) enum AuthGate {
    /// Safe to install the target's stored credentials as-is: a third-party
    /// (api-key) profile, an OAuth token with real life left, or a profile whose
    /// live `clauth start` session keeps its own chain fresh.
    Ready,
    /// The target's expiring OAuth token was refreshed and the rotated pair
    /// persisted; install the refreshed credentials.
    Refreshed,
    /// The target's refresh token is revoked/invalid — the profile is marked
    /// `auth_broken` (persisted). The caller MUST NOT install: a dead token in
    /// the Keychain logs out every running `claude` (Incident C).
    Broken,
    /// A transient failure (network/429/5xx, an unwritable rotation lock, or a
    /// poisoned mutex) blocked a needed refresh. Do not install now; retry on a later
    /// tick. The account is NOT quarantined.
    /// Carries the kind so each surface renders it honestly: the CLI names the
    /// HTTP status, the TUI toast and the MCP payload do not, and the retry
    /// advice follows the failure instead of being one hardcoded sentence.
    Transient(crate::format::Transient),
}

/// Pre-install auth gate (AUTH-1 / Incident C). Installing `name`'s stored
/// credentials into the macOS Keychain instantly re-authenticates every running
/// `claude` on this machine, so a dead token must never be installed: this
/// refreshes an expiring OAuth token before install, quarantines a revoked one
/// ([`AuthGate::Broken`]), and passes healthy or third-party targets through.
/// Every branch is pinned by the `gate_*` tests in this module's test file.
///
/// `refresher` is injected so the gate is testable offline (real callers pass
/// [`refresh_result`]). The config mutex is never held across the HTTP refresh,
/// and the per-profile `RotationGuard` wraps the refresh so a live session or
/// sibling worker cannot double-spend the single-use token.
pub(crate) fn ensure_installable(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
) -> AuthGate {
    ensure_installable_with(config, name, refresher, LockWait::Block)
}

/// [`ensure_installable`] for the daemon's main loop, whose 30 s watchdog aborts
/// a loop parked past it: a staged pair or a rolling-token target whose
/// rotation lock another holder has answers
/// [`crate::format::Cause::RotationLockHeld`] instead of waiting the holder
/// out, and the switch retries on a later tick.
pub(crate) fn ensure_installable_from_main_loop(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
) -> AuthGate {
    ensure_installable_with(config, name, refresher, LockWait::NoWaitInstall)
}

fn ensure_installable_with(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
    wait: LockWait,
) -> AuthGate {
    // A staged pair is the chain head while the store lags behind it, and every
    // verdict below ends in the caller installing the STORE: land it first,
    // under the guard, or refuse the install. Judged lock-free first, so an
    // absent or inert stage never takes the guard at all.
    if stage_needs_landing(name) {
        let guard = match take_rotation_guard(name, wait) {
            Ok(guard) => guard,
            Err(busy) => return AuthGate::Transient(busy),
        };
        if let Err(refused) = land_stage_for_gate(config, name, &guard) {
            return AuthGate::Transient(refused);
        }
    }
    // CLA-ROLL: rolling-token profiles own their entire install story in
    // [`rolling_install_gate`] — every sidecar state (fresh/stale/absent/
    // mis-filled/dead-chain) is decided there, so no rolling-token profile can fall
    // through a vanilla path and install the shared rotating pair.
    if profile_rolling_token(config, name) {
        return rolling_install_gate(config, name, refresher, AUTH_GATE_GRACE_MS, wait);
    }
    vanilla_install_gate(config, name, refresher)
}

/// The pre-CLA-ROLL gate, byte-for-byte: static session-token profiles gate on
/// the token's clock; vanilla OAuth profiles refresh-if-expiring under the
/// rotation guard.
fn vanilla_install_gate(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
) -> AuthGate {
    // CLA-SPLIT: a session-token profile installs its STATIC long-lived token
    // — there is no chain to refresh before install, and a stale/broken
    // usage-side OAuth pair (what `oauth_shape` + `auth_broken` describe)
    // must not bench an account whose session token is perfectly usable.
    // The token's own clock is the one thing worth checking: there is no
    // refresh chain to probe or repair, so a clock-dead static token would
    // otherwise install as-is and sign every session out (Incident C shape).
    match crate::claude::session_token_status(name) {
        Some(crate::claude::SessionTokenStatus::LongLived(expires_at)) => {
            // The SAME grace as every other verdict on these bytes
            // (`AUTH_GATE_GRACE_MS` = CC's five-minute refresh threshold =
            // the backup-restore rule): a mint inside that window installs
            // into a client already trying to refresh a refresh-less
            // credential, which signs the session out moments later — so
            // "identical bytes, identical verdict" has to include the one arm
            // that INSTALLS a mint, or `clauth static-token` calls a file
            // EXPIRED that the very next switch serves happily.
            let clock_dead = expiring(expires_at, false);
            if clock_dead {
                logline!(
                    "clauth: '{name}' long-lived token has expired (or sits inside Claude \
                     Code's own five-minute refresh window) — re-mint with \
                     `claude setup-token` (clauth login {name} --setup-token)"
                );
                return AuthGate::Broken;
            }
            return AuthGate::Ready;
        }
        // #53 review: the split engages only for a token that actually IS
        // long-lived. A sidecar holding a rotating pair is a mis-fill —
        // installing it would front sessions with a dies-in-hours token and
        // no refresher, so it is IGNORED (credentials.json installs below,
        // exactly as if the sidecar weren't there) and called out here, the
        // per-switch chokepoint, rather than on every hot-path stat.
        Some(crate::claude::SessionTokenStatus::NotLongLived) => {
            logline!(
                "clauth: '{name}' session-token.json holds a rotating pair (refresh \
                 token present), not a long-lived mint — ignoring it; re-capture \
                 with `clauth login {name} --setup-token`"
            );
        }
        None => {}
    }
    // Cheap pre-check WITHOUT the rotation guard: non-OAuth and
    // comfortably-live tokens install as-is. Token data read here is
    // discarded — only the post-guard re-read may feed the refresher (a
    // pre-guard snapshot can go stale the moment a sibling rotation runs).
    match oauth_shape(config, name) {
        Err(gate) => return gate,
        Ok((expires_at, _, _, flagged)) if !expiring(expires_at, flagged) => {
            return AuthGate::Ready;
        }
        Ok(_) => {}
    }

    // RotationGuard across the HTTP window (single-use double-spend guard),
    // acquired with no config lock held. Contention does NOT land in the `else`
    // below: `acquire` blocks on the flock, so a sibling worker or live session
    // on this chain makes us wait. Only creating/opening the lock file can fail.
    let Ok(guard) = RotationGuard::acquire(name) else {
        return AuthGate::Transient(rotation_lock_unavailable(name));
    };
    // macOS only, same mechanism as the other rotation legs: a switch TARGET can
    // carry its own live `clauth start` session whose CC reads a Keychain item
    // clauth can't write (`runtime::rotation_blocked_by_live_session`).
    //
    // This RELOCATES the spend, it does not avoid it. Reaching this line means
    // the token is inside the grace — which IS Claude Code's own 5-minute
    // refresh threshold — or `auth_broken`, so installing as-is starts a
    // Claude Code already inside its refresh window: it refreshes on its first
    // request and spends the very chain the other session holds. What the
    // refusal buys is that the spend happens in a process that CAN write the
    // item its reader consults, so the loser is a token rather than a
    // signed-out session. Keep it for that, not for a spend that isn't
    // happening.
    if crate::runtime::rotation_blocked_for(name) {
        return AuthGate::Ready;
    }
    gate_under_guard(config, name, refresher, &guard, AUTH_GATE_GRACE_MS)
}

/// How [`rolling_install_gate`] and [`ensure_installable`]'s stage lander take
/// the profile's rotation lock.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LockWait {
    /// Park behind an in-flight rotation. The switch/arm paths: a session
    /// start would rather wait a rotation out than install around it, and
    /// `acquire`'s blocking is what makes their pre/post-guard re-reads exact.
    Block,
    /// Never park. The scheduler's re-stamp leg runs INLINE on the tick
    /// thread, and this gate's own acquisition carries no deadline — a `clauth
    /// start` holding the lock across its recursive `~/.claude` copy would stall
    /// every account's poll while the heartbeat (stamped in the main loop)
    /// stays fresh. `runtime::ROTATION_LOCK_TIMEOUT` is no help here: it bounds
    /// the SESSION START's wait, not this one, and it waits tens of seconds anyway,
    /// which is a poll tick's whole budget many times over. A held lock returns
    /// Transient instead; the holder's own path re-stamps, or the scan retries
    /// in minutes on an hours-wide horizon.
    NoWait,
    /// Never park, but keep [`Self::Block`]'s install arms: the daemon main
    /// loop's switch gate ([`ensure_installable_from_main_loop`]), whose stage
    /// lander and rolling gate answer a held lock with the busy cause because a
    /// wait there runs into the 30 s watchdog, while the switch still has an
    /// install to make.
    NoWaitInstall,
}

/// Take `name`'s rotation lock as `wait` says. On the [`LockWait::Block`] path
/// `acquire` BLOCKS on the flock, so its error arm is a filesystem or
/// permissions problem under `~/.clauth` and never contention; on the
/// no-wait paths a held lock IS contention, and gets its own cause.
fn take_rotation_guard(
    name: &ProfileName,
    wait: LockWait,
) -> std::result::Result<RotationGuard, crate::format::Transient> {
    match wait {
        LockWait::Block => {
            RotationGuard::acquire(name).map_err(|_| rotation_lock_unavailable(name))
        }
        LockWait::NoWait | LockWait::NoWaitInstall => match RotationGuard::try_acquire(name) {
            Ok(Some(guard)) => Ok(guard),
            Ok(None) => Err(rotation_lock_held(name)),
            Err(_) => Err(rotation_lock_unavailable(name)),
        },
    }
}

/// CLA-ROLL: the complete install gate for a rolling-token profile. Every
/// sidecar state is decided here:
///
///   * mis-filled sidecar → healed (evidence quarantined, static mint
///     restored) when a LIVE backup exists; a repair that raced ahead of us
///     rejoins the normal table below. With nothing live to restore the
///     split stays disengaged (loud), and what happens next follows `wait`:
///     the install paths (`Block`, `NoWaitInstall`) install through the SAME
///     plain gate a non-rolling mis-fill takes — never silently — while the
///     NoWait leg answers
///     Transient with the mis-fill's own cause, because the plain gate's
///     acquire blocks and a disengaged split holds no re-stamp work anyway;
///   * fresh rolling token (or freshly restored mint) → install as-is, no locks;
///   * stale or absent → serialized under the profile's RotationGuard for the
///     whole read-and-restamp (a concurrent rotation's newer stamp can no
///     longer be clobbered by an older cloned token), stamped from the stored
///     chain when comfortable (no spend) or through the guarded refresh
///     (whose persist re-stamps via the rotation hook);
///   * live session on the ROTATING PAIR (started inside an arming window,
///     before any sidecar existed) → refuse to spend: refreshing would
///     revoke the chain under that session, the exact death the split
///     prevents. Decided by [`crate::runtime::rotation_blocked_for`], which
///     reads what each live session LAUNCHED on, so a session already running
///     on a refresh-less bearer never blocks — it holds nothing to strand;
///   * terminally dead chain → restore the static mint (Ready, degraded)
///     else Broken.
fn rolling_install_gate(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
    fresh_horizon_ms: i64,
    wait: LockWait,
) -> AuthGate {
    use crate::claude::SessionTokenStatus;
    if matches!(
        crate::claude::session_token_status(name),
        Some(SessionTokenStatus::NotLongLived)
    ) {
        match crate::claude::heal_misfilled_sidecar(name) {
            Ok(crate::claude::HealOutcome::Healed) => logline!(
                "clauth: '{name}' mis-filled sidecar quarantined; static mint restored \
                 (the rolling token re-arms on the next rotation)"
            ),
            // A concurrent repair — or whatever writes the sidecar — already
            // resolved the mis-fill. Fall through to the normal rolling table,
            // which re-reads the sidecar as it now is. (The old bool folded
            // this into "no backup" and sent a healthy sidecar down the
            // vanilla path.)
            Ok(crate::claude::HealOutcome::NotMisfilled) => {}
            Ok(crate::claude::HealOutcome::NoLiveBackup) => {
                logline!(
                    "clauth: '{name}' sidecar is mis-filled and no live static backup exists \
                     to restore — the split stays disengaged; re-capture with \
                     `clauth login {name} --setup-token`"
                );
                return match wait {
                    // The switch/arm paths keep the pre-split behavior: the
                    // profile installs through the SAME plain gate a
                    // non-rolling mis-fill takes.
                    LockWait::Block | LockWait::NoWaitInstall => {
                        vanilla_install_gate(config, name, refresher)
                    }
                    // The vanilla gate's own acquire BLOCKS, which is exactly
                    // what this axis exists to keep off the tick thread — and
                    // the work it would do there (install or refresh the
                    // ROTATING PAIR) is not re-stamp work at all. Permanent
                    // until an operator re-captures, so the scheduler paces it
                    // on the re-login leash.
                    LockWait::NoWait => AuthGate::Transient(sidecar_misfilled(name)),
                };
            }
            Err(e) => {
                logline!("clauth: '{name}' mis-filled sidecar could not be quarantined ({e:#})");
                return AuthGate::Transient(sidecar_repair_transient(name, &e));
            }
        }
    }
    // Freshness is ROLLING-shaped freshness: a sidecar that CLASSIFIES as a
    // rolling bearer ([`crate::claude::sidecar_kind_of`]) with real life left.
    // A static MINT is deliberately never "fresh" here — on a rolling-token
    // profile it is the live *fallback*, and the gate's job is to supersede it
    // with a plan-capable rolling bearer (the bug that made this explicit: a
    // mint's far-future expiry read as fresh, so arming never stamped
    // anything). A live mint also means the profile is ARMED
    // (`has_session_token` true), so every degrade path below can fall back
    // to Ready on it rather than deferring the switch.
    let rolling_fresh = || {
        matches!(crate::claude::sidecar_summary(name),
            Some((crate::claude::SidecarKind::Rolling, oauth))
                if oauth.expires_at.is_some_and(|e| !horizon_expiring(Some(e), false, fresh_horizon_ms)))
    };
    let sidecar_live = |st: Option<SessionTokenStatus>| matches!(st, Some(SessionTokenStatus::LongLived(exp)) if !expiring(exp, false));
    if rolling_fresh() {
        return AuthGate::Ready;
    }
    // Mint-shaped, stale, or absent: everything below mutates the sidecar or
    // the chain, so it serializes with rotations on the cross-process guard.
    let guard = match take_rotation_guard(name, wait) {
        Ok(guard) => guard,
        Err(busy) => return AuthGate::Transient(busy),
    };
    // Everything below stamps the sidecar from the stored chain or spends it,
    // so a staged pair lands first (the arm and re-stamp legs reach here
    // without `ensure_installable`'s own lander).
    if let Err(refused) = land_stage_for_gate(config, name, &guard) {
        return AuthGate::Transient(refused);
    }
    // The rotation we just serialized with may have re-stamped it already.
    if rolling_fresh() {
        return AuthGate::Ready;
    }
    // The FLAG is re-read from DISK under the guard: the pre-guard routing
    // that chose this gate can be a full clear older than the wait — a
    // `static-token --clear` holding this same guard disarms the profile,
    // takes the sidecar and the preserved mint, and releases. Stamping from
    // the stale routing (an in-memory config the clear's own process never
    // touches) would land a fresh rolling bearer on the profile the operator
    // just cleared, with the flag now off so nothing ever re-stamps it: a
    // dies-in-hours credential with no exit. Disk is what the clear wrote, so
    // disk decides; an unreadable profile keeps the pre-guard routing rather
    // than letting an ~/.clauth hiccup break the arm this leg exists for.
    if matches!(crate::profile::load_profile(name), Ok(p) if !p.rolling_token) {
        return match wait {
            // The switch-in path still has an install to make — the same
            // plain gate a never-armed profile takes. The guard drops FIRST:
            // the vanilla gate blocks on its own acquire of this same lock.
            LockWait::Block | LockWait::NoWaitInstall => {
                drop(guard);
                vanilla_install_gate(config, name, refresher)
            }
            // The scheduler leg has nothing to re-stamp anymore; its still-due
            // re-read sees the sidecar gone and drops the pacing hold.
            LockWait::NoWait => AuthGate::Ready,
        };
    }
    match roll_from_stored_chain(config, name, &guard, fresh_horizon_ms) {
        RollAttempt::Stamped => return AuthGate::Ready,
        // A stamp WRITE failure with a live sidecar still installs what that
        // sidecar holds (degraded but serving — the next rotation retries the
        // stamp); with no live sidecar it must not fall anywhere (the refresh
        // leg would early-Ready on the comfortable chain without stamping).
        RollAttempt::WriteFailed(e) => {
            return if sidecar_live(crate::claude::session_token_status(name)) {
                logline!(
                    "clauth: '{name}' rolling-token write failed ({e:#}); sessions stay on {}",
                    serving_desc(name)
                );
                AuthGate::Ready
            } else {
                logline!("clauth: '{name}' rolling-token write failed ({e:#})");
                AuthGate::Transient(sidecar_repair_transient(name, &e))
            };
        }
        // Permanent until a re-login: never fall through to the refresh leg,
        // which would spend a rotation to arrive at the same refusal. But a
        // LIVE sidecar still installs — before this verdict existed, the same
        // chain shape reached `stamp_rolling_token`'s bail and the WriteFailed
        // arm's `sidecar_live` fallback served the mint; losing that turned a
        // profile with a perfectly installable mint into a hard switch refusal
        // (verification fleet, round 3).
        RollAttempt::GrantUnusable => {
            return if sidecar_live(crate::claude::session_token_status(name)) {
                logline!(
                    "clauth: '{name}' usage chain's recorded grant cannot mint a rolling \
                     bearer (re-run `clauth login {name}` to record it); installing {}",
                    serving_desc(name)
                );
                AuthGate::Ready
            } else {
                AuthGate::Transient(rolling_grant_unrecorded(name))
            };
        }
        RollAttempt::ChainStale => {}
    }
    // A live session launched on the ROTATING PAIR — it started before any
    // sidecar existed, so `install_source_path` handed it credentials.json.
    // Its own swap poll converges it onto the sidecar in place; until that
    // lands, spending the refresh here would fail that session's next refresh
    // (`invalid_grant`) and can blank its own Keychain item. Asked through
    // `rotation_blocked_for` rather than re-derived, so this leg inherits the
    // one place that decision lives (and, with it, the fact that the whole
    // refusal is macOS-only: elsewhere the session reads the very file a
    // rotation rewrites).
    if crate::runtime::rotation_blocked_for(name) {
        return AuthGate::Transient(live_session_on_rotating_chain(name));
    }
    let gate = gate_under_guard(config, name, refresher, &guard, fresh_horizon_ms);
    match gate {
        // The refresh persisted through the rotation hook, which stamped the
        // sidecar as a side effect.
        AuthGate::Refreshed => AuthGate::Refreshed,
        // A terminally dead usage chain degrades to the static mint —
        // restored from backup, or already sitting in the sidecar — instead
        // of benching an account whose sessions could still run.
        AuthGate::Broken => {
            // A restore failure is a filesystem problem worth a line of its
            // own, not a silent `false`: on the daemon's dead-chain degrade it
            // is the difference between "no backup existed" and "the backup is
            // there and could not be installed".
            if let Err(e) = crate::claude::restore_static_mint(name) {
                logline!("clauth: '{name}' static-mint restore failed ({e:#})");
            }
            // `sidecar_live` alone decides — never `restored ||`. A restore
            // reports true for installing the backup, not for the backup being
            // ALIVE, and short-circuiting the clock test here would put an
            // expired mint straight into the live slot on exactly the path
            // this rescue exists for (the same Incident C shape the vanilla
            // gate's clock check guards 200 lines up). `restore_static_mint`
            // refuses an expired backup outright, so in practice a restore
            // that ran IS live — but the liveness read below is the invariant,
            // not that coupling.
            if sidecar_live(crate::claude::session_token_status(name)) {
                logline!(
                    "clauth: '{name}' usage chain is dead — sessions degrade to {} \
                     (`clauth login {name}` revives the chain and the rolling token)",
                    serving_desc(name)
                );
                AuthGate::Ready
            } else {
                AuthGate::Broken
            }
        }
        // Transient chain trouble with a live sidecar: install what it holds
        // now (degraded but serving) rather than deferring a switch a healthy
        // static token could carry; the rolling token self-heals on a later rotation.
        AuthGate::Transient(e) => {
            if sidecar_live(crate::claude::session_token_status(name)) {
                logline!(
                    "clauth: '{name}' chain refresh hit a transient failure ({}); \
                     installing {} while the rolling token retries",
                    e.text_with_status(),
                    serving_desc(name)
                );
                AuthGate::Ready
            } else {
                AuthGate::Transient(e)
            }
        }
        // Ready from the vanilla leg = a sibling refreshed under the guard
        // window; its persist ran the stamp hook.
        AuthGate::Ready => AuthGate::Ready,
    }
}

/// CLA-ROLL: arm or re-stamp `name`'s rolling sidecar right now — the CLI-enable
/// path. Same decision table as [`rolling_install_gate`] (the CLI pre-clears a
/// mis-fill by quarantining it, so the vanilla fall-through leg is
/// unreachable here in practice — and if raced back in, `Ready` from the
/// vanilla gate still reports arming failure via the sidecar check).
pub(crate) fn arm_rolling_token(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
) -> Result<()> {
    match rolling_install_gate(config, name, refresher, AUTH_GATE_GRACE_MS, LockWait::Block) {
        AuthGate::Ready | AuthGate::Refreshed => {
            if crate::claude::has_session_token(name) {
                Ok(())
            } else {
                anyhow::bail!(
                    "'{name}' could not arm the rolling sidecar (a mis-filled sidecar with no \
                     backup?). Re-capture with `clauth login {name} --setup-token`, or clear \
                     the sidecar, then re-run"
                )
            }
        }
        AuthGate::Broken => {
            anyhow::bail!(
                "'{name}' usage chain is dead · run `clauth login {name}` first, then re-run"
            )
        }
        // CLI surface: `text_with_status` is the flavor that names the HTTP
        // status, since stderr has no companion log to read it out of.
        AuthGate::Transient(e) => Err(anyhow::anyhow!("{}", e.text_with_status())),
    }
}

/// CLA-ROLL: how much life a rolling sidecar must keep before the daemon's
/// re-stamp leg leaves it alone. Rolling bearers die in hours (they clone the
/// usage chain's access-token expiry); re-stamping this far ahead keeps a
/// running session's bearer alive across daemon idle gaps, spent-window poll
/// parking, and machine sleep — the failure this exists for was a sidecar quietly hitting
/// its ~7h clock while re-stamps waited on a rotation that never came.
pub(crate) const ROLLING_RESTAMP_HORIZON_MS: i64 = 2 * 60 * 60 * 1000;

/// CLA-ROLL due predicate for the scheduler's re-stamp leg: an armed,
/// exp-carrying sidecar inside [`ROLLING_RESTAMP_HORIZON_MS`] of death — or a
/// mis-fill, which is due NOW: its clock is irrelevant because the CONTENT is
/// the defect, switches refuse to install it, and the gate behind this
/// predicate is the only leg a running daemon has that can repair it (heal
/// from the preserved mint, or report the no-backup state on its own cause —
/// on which the scheduler's credential-file watch then makes the operator's
/// re-mint the release). Without this arm a mis-filled sidecar beside a
/// healthy backup sat unrepaired forever on any profile nobody switched to.
/// Absent sidecars (arming is switch/rotation work) and exp-less claims stay
/// not-due.
pub(crate) fn rolling_sidecar_restamp_due(name: &ProfileName, now: i64) -> bool {
    match crate::claude::session_token_status(name) {
        Some(crate::claude::SessionTokenStatus::LongLived(Some(exp))) => {
            exp <= now + ROLLING_RESTAMP_HORIZON_MS
        }
        Some(crate::claude::SessionTokenStatus::NotLongLived) => true,
        _ => false,
    }
}

/// CLA-ROLL: the scheduler-leg re-stamp for one rolling-token profile — the
/// same complete decision table as the switch-in gate (no-spend re-stamp from
/// a comfortable chain / guarded refresh / mint degrade), but judged against
/// the generous [`ROLLING_RESTAMP_HORIZON_MS`] instead of the switch gate's
/// minutes-tight grace. For the ACTIVE profile a no-spend re-stamp must also
/// reach the live slot: the macOS Keychain (a `Refreshed` outcome already
/// mirrored through the rotation hook; the running `claude` re-reads the
/// Keychain per request) and the live FILE on every OS, through the
/// file-layer follower below — same refresh-less content belt as the hook:
/// nothing carrying a refresh token can ship through the rolling path.
pub(crate) fn restamp_rolling_token(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
) -> AuthGate {
    // The sidecar's PRE-re-stamp bearer, captured before the gate below
    // overwrites it — what the live-slot followers recognize against: the
    // macOS Keychain mirror's foreign gate and the file-layer follower's.
    // A rolling bearer changes on every stamp, so recognition needs the
    // token being replaced, never the one being written.
    let previous_bearer = crate::claude::install_source_path(name)
        .ok()
        .and_then(|p| crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(&p).ok())
        .and_then(|c| {
            c.access_token()
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        });
    let gate = rolling_install_gate(
        config,
        name,
        refresher,
        ROLLING_RESTAMP_HORIZON_MS,
        LockWait::NoWait,
    );
    // A ROLLING-classified sidecar now clear of the horizon = the no-spend
    // re-stamp just landed (the Refreshed and mint-degrade paths log at their
    // source, and a mint left in place classifies out here rather than being
    // horizon-guessed at).
    if matches!(gate, AuthGate::Ready) {
        let now = now_ms() as i64;
        if matches!(
            crate::claude::sidecar_summary(name),
            Some((crate::claude::SidecarKind::Rolling, oauth))
                if oauth.expires_at.is_some_and(|exp| exp > now + ROLLING_RESTAMP_HORIZON_MS)
        ) {
            logline!("clauth: re-stamped '{name}' session token ahead of its expiry");
        }
    }
    // In-process switches stay excluded for the whole is-active check + write
    // by holding the config mutex across it — the `apply_rotated_tokens_locked`
    // mirror discipline (the state FLOCK is what must never span the
    // `/usr/bin/security` subprocess; the config mutex is expected to).
    #[cfg(target_os = "macos")]
    if matches!(gate, AuthGate::Ready)
        && crate::keychain::enabled()
        && let Ok(cfg) = config.lock()
        && cfg.is_active(name)
        && let Ok(path) = crate::claude::install_source_path(name)
        && let Ok(creds) =
            crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(&path)
        && creds.refresh_token().is_none()
    {
        // CLA-SPLIT foreign gate, same rule as the rotation hook's split
        // mirror: `Keep::Everything` preserves the item's sibling blocks, so
        // the item's login must be one clauth put there first (the file
        // layer is not evidence once CC migrates into the Keychain).
        // Candidates: the pre-re-stamp bearer and the bearer being written.
        let incoming = creds.access_token().map(str::to_string);
        let candidates: Vec<&str> = [previous_bearer.as_deref(), incoming.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        match crate::keychain::item_login_state(&candidates) {
            // Corrupt proceeds with the write: the mirror's own read leg
            // quarantines the truncated bytes and the write heals the item.
            crate::keychain::ItemLoginState::Ours | crate::keychain::ItemLoginState::Corrupt => {
                if let Err(e) = crate::keychain::keychain_mirror_rotation(&creds) {
                    logline!("clauth: re-stamped '{name}' but the Keychain mirror failed: {e:#}");
                }
            }
            crate::keychain::ItemLoginState::NotOurs => logline!(
                "clauth: re-stamped '{name}' but the macOS Keychain login is not one clauth \
                 recognizes (an out-of-band re-login, or a mirror write that failed a rotation \
                 back). Keychain left untouched; {}",
                crate::format::RESOLVE_IN_TUI
            ),
            crate::keychain::ItemLoginState::Unreadable(e) => logline!(
                "clauth: re-stamped '{name}' but the macOS Keychain item could not be read to \
                 check its login ({e}); mirror skipped, the previous rolling bearer keeps \
                 serving until it expires. Run `clauth {name}` to reinstall"
            ),
        }
    }
    // #104: the file-layer counterpart of the Keychain mirror above — the
    // Linux live slot IS the file, and a regular file CC wrote (replacing
    // clauth's symlink) must follow the re-stamp or every session keeps
    // presenting the revoked bearer. Ready-with-a-changed-sidecar is this
    // leg's own no-spend stamp; a Refreshed outcome stamps through the
    // rotation hook, whose own follower leg covers the live slot. The
    // re-read legs fail LOUD: a silent skip here reproduces the exact #104
    // symptom through the fix's own failure path.
    if matches!(gate, AuthGate::Ready)
        && let Ok(cfg) = config.lock()
    {
        if !cfg.is_active(name) {
            return gate;
        }
        drop(cfg);
        let followed = match crate::claude::install_source_path(name).and_then(|path| {
            crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(&path)
        }) {
            Ok(creds) => {
                let Some(oauth) = creds.claude_ai_oauth.as_ref() else {
                    return gate;
                };
                if previous_bearer.as_deref() == Some(oauth.access_token.as_str()) {
                    return gate;
                }
                let new_access = Some(oauth.access_token.clone());
                let candidates: Vec<&str> = [previous_bearer.as_deref(), new_access.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect();
                crate::claude::follow_live_file_login(&candidates, oauth)
            }
            Err(e) => {
                logline!(
                    "clauth: re-stamped '{name}' but re-reading the sidecar failed: {e:#}. \
                     Live file left untouched; run `clauth {name}` to reinstall"
                );
                return gate;
            }
        };
        match followed {
            Ok(crate::claude::LiveSlotFollow::NotOurs) => logline!(
                "clauth: re-stamped '{name}' but the live .credentials.json login is not one \
                 clauth recognizes (an out-of-band re-login, or a follow write that failed a \
                 re-stamp back). Live file left untouched; {}",
                crate::format::RESOLVE_IN_TUI
            ),
            Ok(crate::claude::LiveSlotFollow::Unreadable(e)) => logline!(
                "clauth: re-stamped '{name}' but the live .credentials.json could not be \
                 parsed to check its login ({e}); live file left untouched. Run \
                 `clauth {name}` to reinstall"
            ),
            Err(e) => logline!(
                "clauth: re-stamped '{name}' but following the live .credentials.json failed: \
                 {e:#}. A running claude signs out when its old token expires; run \
                 `clauth {name}` to reinstall"
            ),
            Ok(_) => {}
        }
    }
    gate
}

/// CLA-ROLL: whether `name` has the rolling token enabled. A poisoned config
/// mutex or unknown profile reads `false` — the static/vanilla gates apply.
fn profile_rolling_token(config: &crate::profile::ConfigHandle, name: &ProfileName) -> bool {
    config
        .lock()
        .ok()
        .and_then(|c| c.find(name).map(|p| p.rolling_token))
        .unwrap_or(false)
}

/// CLA-ROLL: name what the sidecar is actually serving, for the degrade-path
/// loglines. `sidecar_live` only proves "refresh-less with more than a grace
/// window left" — on the re-stamp leg, whose horizon is hours wide, that can
/// be a rolling bearer in its last two hours just as well as the year-scale
/// mint, and a log that says "the mint" over a bearer dying within the hour is
/// the comfortable-looking lie this feature exists to remove.
fn serving_desc(name: &ProfileName) -> &'static str {
    match crate::claude::sidecar_summary(name) {
        Some((crate::claude::SidecarKind::Mint, _)) => "the static long-lived mint",
        // Unreachable from the degrade paths (every caller guards on
        // `sidecar_live`, which requires a refresh-less LongLived read), but a
        // mis-fill must never be DESCRIBED as a serving bearer if one arrives.
        Some((crate::claude::SidecarKind::Misfilled, _)) => {
            "nothing — its sidecar is mis-filled and the split is disengaged"
        }
        _ => "its last rolling bearer, until that expires",
    }
}

/// Outcome of [`roll_from_stored_chain`].
enum RollAttempt {
    /// Sidecar re-stamped from the stored chain — no refresh spent.
    Stamped,
    /// The stored chain is itself expiring/broken (or absent): the caller
    /// routes to the guarded refresh leg, whose persist re-stamps the sidecar
    /// via the rotation hook.
    ChainStale,
    /// The chain is healthy but the sidecar write failed — the caller must
    /// NOT fall through (the refresh leg would early-Ready on the comfortable
    /// chain without re-stamping a stale sidecar).
    WriteFailed(anyhow::Error),
    /// The chain's RECORDED grant would classify as a mint, so
    /// `stamp_rolling_token` refuses it — permanently, until a re-login
    /// records the real grant. Its own arm rather than `WriteFailed`, because
    /// rendering it as a filesystem problem with a retry hint points the
    /// operator at `~/.clauth` permissions when the only fix is
    /// `clauth login`.
    GrantUnusable,
}

/// CLA-ROLL: re-stamp `name`'s sidecar from the STORED usage chain when its
/// access token is comfortably live — the no-spend path for a stale rolling token
/// at switch time. A standing `auth_broken` routes to `ChainStale` (server-side
/// revocation kills the access token with the chain, so a comfortable clock
/// proves nothing there — same rationale as [`expiring`]'s flag override).
/// `_rotation_guard` witnesses the caller holding the profile's rotation lock
/// for the whole read-and-write: without it, a concurrent rotation's NEWER rolling
/// token could be clobbered by this call's older cloned access token.
fn roll_from_stored_chain(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    _rotation_guard: &RotationGuard,
    fresh_horizon_ms: i64,
) -> RollAttempt {
    let Ok(cfg) = config.lock() else {
        return RollAttempt::ChainStale;
    };
    let flagged = cfg.is_auth_broken(name);
    let chain = cfg
        .find(name)
        .and_then(|p| p.credentials.as_ref())
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .cloned();
    drop(cfg);
    let Some(oauth) = chain else {
        return RollAttempt::ChainStale;
    };
    if horizon_expiring(oauth.expires_at, flagged, fresh_horizon_ms) {
        return RollAttempt::ChainStale;
    }
    // Classified BEFORE the stamp, on the refresh-less projection the stamp
    // would write — the same constructor the stamp itself uses — so the
    // permanent refusal gets its own verdict instead of surfacing as a
    // filesystem-flavored write failure.
    let projected = crate::claude::rolling_projection(&oauth);
    if crate::claude::sidecar_kind_of(&projected) != crate::claude::SidecarKind::Rolling {
        return RollAttempt::GrantUnusable;
    }
    match crate::claude::stamp_rolling_token(name, &oauth) {
        Ok(()) => RollAttempt::Stamped,
        Err(e) => RollAttempt::WriteFailed(e),
    }
}

/// The target's auth shape — `(access-token expiry, refresh token, standing
/// auth_broken flag)` — read under the config lock and released before
/// returning, so no caller ever holds the mutex across an HTTP refresh. `Err`
/// carries the gate verdict for the non-OAuth / unknown-profile / poisoned
/// cases.
#[allow(
    clippy::type_complexity,
    reason = "one-shot tuple, named at both call sites"
)]
fn oauth_shape(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
) -> std::result::Result<(Option<i64>, Option<String>, Option<String>, bool), AuthGate> {
    let Ok(cfg) = config.lock() else {
        // A poisoned mutex means another thread panicked; it does not clear on
        // its own, so a retry hint would be a lie.
        return Err(AuthGate::Transient(crate::format::Transient::new(
            crate::format::Cause::InternalLock,
            crate::format::Retry::Stated,
        )));
    };
    let Some(profile) = cfg.find(name) else {
        // Unknown profile: nothing to gate — the switch itself surfaces
        // "Profile not found".
        return Err(AuthGate::Ready);
    };
    if !profile.is_oauth() {
        // Third-party (api-key) profiles carry no OAuth token to expire.
        return Err(AuthGate::Ready);
    }
    Ok((
        profile.access_token_expires_at(),
        profile.refresh_token().map(str::to_string),
        profile.scopes_joined(),
        cfg.is_auth_broken(name),
    ))
}

/// Unknown expiry → treated as not-expiring (mirrors `auto_start_kick`):
/// install as-is and let the lazy 401→rotate path handle a surprise expiry.
/// A standing `auth_broken` flag overrides the clock: the chain's last refresh
/// terminally failed, so a still-future `expires_at` proves nothing
/// (server-side revocation outlives the stored clock). Route it through the
/// refresher — a recovered chain comes back `Refreshed` and lifts the flag, a
/// dead one confirms `Broken`.
fn expiring(expires_at: Option<i64>, flagged: bool) -> bool {
    horizon_expiring(expires_at, flagged, AUTH_GATE_GRACE_MS)
}

/// [`expiring`] with a caller-chosen margin. The switch gates keep the tight
/// [`AUTH_GATE_GRACE_MS`]; the CLA-ROLL re-stamp leg passes
/// [`ROLLING_RESTAMP_HORIZON_MS`] so a rolling bearer is renewed HOURS before its
/// clock death, not minutes — the margin that keeps a running session alive
/// across daemon idle gaps and machine sleep.
fn horizon_expiring(expires_at: Option<i64>, flagged: bool, horizon_ms: i64) -> bool {
    flagged || expires_at.is_some_and(|exp| (now_ms() as i64) + horizon_ms >= exp)
}

/// Reconcile the in-memory profile with the on-disk store; the `_guard`
/// witness proves the [`RotationGuard`] is held, which makes the disk read
/// stable. A cross-process peer (the daemon, a second clauth) rotates and
/// persists under this same flock, and a caller that loaded config from disk
/// once (CLI, MCP) can hold a snapshot predating that write. Tokens are opaque
/// and no writer rewinds the store (see the scheduler's `fresher_disk_pair`),
/// so a stored refresh token that DIFFERS from the in-memory one proves
/// someone advanced the single-use chain: adopt the disk pair, and lift a
/// stale quarantine — the chain is alive under someone else's advance
/// (mirrors `carry_external_rotation`; a wrong lift self-corrects when the
/// carried pair's own refresh 400s). Unreadable or tokenless disk state is a
/// no-op: the in-memory shape stays the best available truth. Only the
/// state-flock failure is an error — proceeding past it would refresh from
/// the stale in-memory pair: a double-spend of the single-use token a
/// sibling just advanced, and a re-quarantine of a login the disk pair
/// proves alive.
pub(crate) fn adopt_disk_rotation(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    _guard: &RotationGuard,
) -> Result<()> {
    let Ok(disk) = crate::profile::load_profile(name) else {
        return Ok(());
    };
    // The load hands back a live stage's pair (the chain head, unspent by
    // definition) or the store's, which is adoptable only while unspent: a
    // store holding a token the server consumed is a rewound chain, never an
    // advance, and an unreadable record proves neither.
    let Some(disk_refresh) = disk.refresh_token() else {
        return Ok(());
    };
    if SpentRecord::read(name).spent(disk_refresh) != Some(false) {
        return Ok(());
    }
    {
        let Ok(mut cfg) = config.lock() else {
            return Ok(());
        };
        let Some(profile) = cfg.find_mut(name) else {
            return Ok(());
        };
        if profile.refresh_token() == disk.refresh_token() {
            return Ok(());
        }
        let creds = disk.credentials.into_inner();
        // The slot write goes through the witness, so take the flock for the
        // in-memory adoption itself — the established `config` → state order.
        with_state_lock(|held| {
            profile.set_credentials(creds, held);
            Ok(())
        })?;
    }
    mark_auth_broken(config, name, false);
    Ok(())
}

/// Map a failed adoption flock to its Transient — contention and fault are
/// different verdicts, the same split `sidecar_repair_transient` makes for
/// the repair leg. `with_state_lock` fails on a bounded cross-process flock
/// timeout ([`crate::lock::StateLockTimeout`]) or an IO fault, and on macOS
/// a sibling process can hold that flock across `/usr/bin/security`
/// shell-outs sharing a 20 s aggregate budget (`lock::SUBPROCESS_BUDGET`,
/// each invocation capped at 10 s) — a slow Keychain in ANOTHER process
/// surfaces here as a timeout.
fn adopt_lock_transient(name: &ProfileName, e: &anyhow::Error) -> crate::format::Transient {
    if e.chain()
        .any(|c| c.downcast_ref::<crate::lock::StateLockTimeout>().is_some())
    {
        return crate::format::Transient::new(
            crate::format::Cause::StateLockBusy(name.to_string()),
            crate::format::Retry::Wait,
        );
    }
    crate::format::Transient::new(
        crate::format::Cause::StateLockUnavailable(name.to_string()),
        // The cause names its own next step; a second one contradicts it.
        crate::format::Retry::Stated,
    )
}

/// The refresh leg; the `guard` witness proves the [`RotationGuard`] is held.
/// First adopts a cross-process rotation from disk ([`adopt_disk_rotation`];
/// an adoption that could not take the state flock refuses the gate as
/// Transient — never a proceed), then re-reads the auth shape UNDER the guard
/// — between the pre-check and guard acquisition a sibling rotation
/// (in-process or peer) may have spent the single-use refresh token and
/// persisted a new pair, and refreshing from that stale snapshot would 400
/// and wrongly quarantine a healthy login. This function takes no token
/// arguments, so post-guard decisions structurally cannot reuse pre-guard
/// data.
fn gate_under_guard(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
    guard: &RotationGuard,
    fresh_horizon_ms: i64,
) -> AuthGate {
    // A failed adoption flock is a refusal, never a no-op: proceeding would
    // spend the already-spent single-use token from the stale in-memory pair.
    if let Err(e) = adopt_disk_rotation(config, name, guard) {
        return AuthGate::Transient(adopt_lock_transient(name, &e));
    }
    let (expires_at, refresh_token, scopes, flagged) = match oauth_shape(config, name) {
        Err(gate) => return gate,
        Ok(shape) => shape,
    };
    if !horizon_expiring(expires_at, flagged, fresh_horizon_ms) {
        // A sibling refreshed while we acquired the guard — the stored pair is
        // fresh; install it as-is instead of double-spending the old chain.
        return AuthGate::Ready;
    }
    let Some(rt) = refresh_token else {
        // Expiring OAuth token with no refresh token — unrecoverable without a
        // re-login.
        mark_auth_broken(config, name, true);
        return AuthGate::Broken;
    };
    let rt = match send_rule(name, &rt, guard) {
        SendRule::Send(rt) => rt,
        // The adoption above already takes an unspent store, so this is a store
        // that moved under the guard (an install): take it, and spend it only
        // when it is itself expiring — it is unspent, so it is sendable.
        SendRule::Adopt((_, adopted)) => {
            if let Err(e) = adopt_disk_rotation(config, name, guard) {
                return AuthGate::Transient(adopt_lock_transient(name, &e));
            }
            match oauth_shape(config, name) {
                Err(gate) => return gate,
                Ok((expires_at, _, _, flagged))
                    if !horizon_expiring(expires_at, flagged, fresh_horizon_ms) =>
                {
                    return AuthGate::Ready;
                }
                Ok(_) => match adopted {
                    Some(adopted) => adopted,
                    None => return AuthGate::Ready,
                },
            }
        }
        SendRule::Quarantine => {
            mark_auth_broken(config, name, true);
            return AuthGate::Broken;
        }
        SendRule::Skip => return AuthGate::Transient(spent_record_unreadable(name)),
    };

    match settle_refresh_answer(config, name, &rt, refresher(&rt, scopes.as_deref()), guard) {
        Ok(minted) => {
            let persist_failed = || {
                AuthGate::Transient(crate::format::Transient::new(
                    crate::format::Cause::PersistFailed(name.to_string()),
                    crate::format::Retry::Wait,
                ))
            };
            match apply_rotated_tokens_locked(
                config,
                name,
                &minted.sent_fp,
                &minted.creds,
                &minted.old_access,
            ) {
                Ok(()) => {}
                // The store moved under the refresh (an install or a log out)
                // and memory now holds it: answer for that login as a fresh
                // gate would, never as a failed save. The server just accepted
                // the old chain, so a standing quarantine describes neither
                // that chain nor whatever replaced it.
                Err(PersistError::Refused { .. }) => {
                    mark_auth_broken(config, name, false);
                    return match oauth_shape(config, name) {
                        Err(gate) => gate,
                        Ok((expires_at, _, _, flagged))
                            if !horizon_expiring(expires_at, flagged, fresh_horizon_ms) =>
                        {
                            AuthGate::Ready
                        }
                        Ok(_) => persist_failed(),
                    };
                }
                Err(PersistError::Failed(_)) => return persist_failed(),
            }
            // A successful refresh clears any prior quarantine.
            mark_auth_broken(config, name, false);
            AuthGate::Refreshed
        }
        Err(e) => {
            // The endpoint's status no longer reaches any refusal copy, so this
            // is where an operator reads it — the daemon's `deferring switch`
            // line and the CLI/TUI/MCP refusals all carry canned text now.
            logline!("clauth: refresh for '{name}' failed: {}", e.log_detail());
            match e {
                RefreshError::Invalid { .. } => {
                    mark_auth_broken(config, name, true);
                    AuthGate::Broken
                }
                RefreshError::Transient(f) => AuthGate::Transient(f.as_refresh_transient()),
            }
        }
    }
}

/// Set or clear a profile's `auth_broken` flag in memory and persist it. The
/// memory flip is unconditional — a refused write must not un-quarantine the
/// account for live readers, since the scheduler's TokenEntry leg reads this
/// flag to skip the refresh spend — and the persist runs on EVERY call, not
/// only on transitions: the memory flag alone cannot tell "already on disk"
/// from "write refused", so any next call through here is the retry that
/// catches disk up (a read-only no-op once it matches). In a live daemon that
/// retry is the CLEAR direction (each successful refresh re-clears) and the
/// switch/install gates; a quarantined profile's own fetch is skipped, so a
/// failed SET stays memory-only until process exit. A refused persist logs
/// one line naming the profile and direction; the transition log stays
/// guarded by `set_auth_broken`'s changed-return, so a retried persist never
/// re-logs. A write never retried before the process exits stays invisible
/// to the next process — the pre-existing semantics of an unwritten flag.
/// Locks `config` (outer) then the state flock (inner) — the established
/// save order.
///
/// The save goes through [`crate::profile::set_auth_broken_persisted`] rather
/// than re-serializing the whole in-memory `AppState`: a daemon leg can hold a
/// config older than a concurrent CLI delete/rename/login, and writing the full
/// stale list would resurrect a deleted profile's row or rewind an edit to some
/// other profile in the same file.
pub(crate) fn mark_auth_broken(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    broken: bool,
) {
    let Ok(mut cfg) = config.lock() else {
        return;
    };
    if cfg.set_auth_broken(name, broken) {
        // Log the transition only — guarded by `set_auth_broken`'s changed-return
        // (pinned by `set_auth_broken_reports_transitions_and_is_idempotent`) so a
        // dropped login leaves one stderr line, never a per-tick repeat.
        if broken {
            // The durable record of the quarantine names the same recovery the
            // live surfaces do: this leg fires for a third-party hybrid too (the
            // scheduler spends any profile holding a refresh token).
            let sentence = third_party_dead_chain_copy(cfg.find(name), name)
                .unwrap_or_else(|| crate::format::login_expired(name).line());
            logline!("clauth: {sentence} (flagged auth_broken)");
        } else {
            logline!("clauth: '{name}' re-authenticated: auth_broken cleared");
        }
    }
    // Persisted on every call so a refused write is retried by the next one —
    // the error used to be discarded here, stranding a quarantine that
    // idempotence then locked in place: memory said broken, the next call
    // early-returned on the unchanged flag, and a restart lost the flag
    // that never reached disk.
    if let Err(e) = crate::profile::set_auth_broken_persisted(name, broken) {
        let direction = if broken { "set" } else { "clear" };
        logline!("clauth: failed to persist auth_broken {direction} for '{name}': {e:#}");
    }
}

#[cfg(test)]
#[path = "../tests/inline/oauth.rs"]
mod tests;
