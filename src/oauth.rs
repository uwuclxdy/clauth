use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::claude::{LinkState, classify_credentials_link};
use crate::lock::with_state_lock;
use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::profile::{
    AccountId, AppConfig, OAuthToken, ProfileName, SlotOps, clear_staged_credentials, save_profile,
    stage_rotated_credentials,
};
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
    if let Some(url) = TOKEN_ENDPOINT_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        return std::borrow::Cow::Owned(url);
    }
    std::borrow::Cow::Borrowed(TOKEN_ENDPOINT)
}

fn messages_endpoint() -> std::borrow::Cow<'static, str> {
    #[cfg(test)]
    if let Some(url) = MESSAGES_ENDPOINT_OVERRIDE
        .lock()
        .ok()
        .and_then(|g| g.clone())
    {
        return std::borrow::Cow::Owned(url);
    }
    std::borrow::Cow::Borrowed(MESSAGES_ENDPOINT)
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
    Invalid(TokenFailure),
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
            Self::Invalid(f) | Self::Transient(f) => f.log_detail(),
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
        return Err(RefreshError::Invalid(TokenFailure::Status(status)));
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

/// Exchange an authorization code (from the interactive loopback login in
/// `oauth_login`) for an OAuth token pair. Uses the same client + HTTP agent as
/// [`refresh_result`], against [`TOKEN_ENDPOINT`] (the `platform.claude.com`
/// host the current Claude Code binary uses), carrying the same axios-mimicking
/// headers. `redirect_uri` MUST byte-match the one sent to the authorize
/// endpoint, and `state` echoes the value round-tripped through the browser.
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

    // Refresh spinner during the round trip, then back to Fetching for the retry
    // kick + the caller's fetch (the kick runs inside the scheduler's fetch leg).
    if let Some(activity) = activity {
        mark_activity(activity, name, ProfileActivity::Refreshing);
    }
    let refreshed = refresh_result(rt, stored_scopes(config, name).as_deref());
    if let Some(activity) = activity {
        mark_activity(activity, name, ProfileActivity::Fetching);
    }
    let tok = match refreshed {
        Ok(t) => t,
        Err(_) => return KickResult::not_opened_with(first_rl),
    };

    let access = tok.access_token.clone();
    let new_refresh = tok.refresh_token.clone();
    // The refresh already spent the old single-use token, so this pair is now the
    // only usable one — carry it back even when the persist below fails, or the
    // caller's live snapshot keeps the dead token and 400s every tick until a
    // restart adopts the staged sidecar. The retry kick may still fail (`opened`
    // false), but a minted pair must always propagate (see `KickResult`).
    let rotated = Some((access.clone(), Some(new_refresh)));
    if apply_rotated_tokens_locked(config, name, tok).is_err() {
        return KickResult {
            opened: false,
            rotated,
            blocked: first_rl,
        };
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
/// across the `/usr/bin/security` shell-out for up to 20 seconds — so a slow
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
pub(crate) fn third_party_dead_chain_copy(
    profile: Option<&crate::profile::Profile>,
    name: &ProfileName,
) -> Option<String> {
    let profile = profile?;
    if crate::claude::has_own_inference_endpoint(profile) {
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
/// `refresh_all` pre-stamps and clears it). The no-refresh-token leg returns
/// [`RotateOutcome::Persisted(false)`] silently.
fn rotate_one_inner(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    activity: Option<&ActivityStore>,
    sender: &OpResultSender,
) -> RotateOutcome {
    let Ok(_rotation_guard) = RotationGuard::acquire(name) else {
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
    // `refresh_result`, not a collapsing wrapper: this leg's `OpResult` becomes a
    // Danger toast, and a dead chain has a different next step from a network
    // blip. Flattening the split here is what put `HTTP 400: {"error":
    // "invalid_grant", …}` on screen while the switch gate and the poll — both
    // matching the variant — showed the canned line.
    let outcome = match refresh_result(&rt, scopes.as_deref()) {
        Ok(tok) => apply_rotated_tokens_locked(config, name, tok),
        Err(e) => {
            logline!("clauth: refresh for '{name}' failed: {}", e.log_detail());
            // This `OpResult`'s only sink is the TUI's Danger toast, whose first
            // line already reads `refresh for '<name>' failed` — so the OAuth
            // arm carries the NEXT STEP alone rather than restating the
            // condition and the account name under it. Both third-party
            // branches restate it: their sentences are owner-ruled copy,
            // rendered whole so the surfaces cannot drift.
            Err(match e {
                RefreshError::Invalid(_) => {
                    anyhow::anyhow!("{}", dead_chain_detail(config, name))
                }
                RefreshError::Transient(f) => {
                    anyhow::anyhow!("{}", f.as_refresh_transient().text())
                }
            })
        }
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

/// Write rotated token fields into an OAuth block. Caller holds the state lock.
fn write_token_fields(oauth: &mut OAuthToken, tok: TokenResponse) {
    oauth.access_token = tok.access_token;
    oauth.refresh_token = Some(tok.refresh_token);
    oauth.expires_at = Some((now_ms() + tok.expires_in * 1000) as i64);
    if let Some(scope) = tok.scope {
        oauth.scopes = Some(scope.split_whitespace().map(String::from).collect());
    }
}

/// Write a rotated token pair into the named profile's OAuth block and persist.
/// Takes `&ConfigHandle` so workers can call from a thread without holding the
/// lock across HTTP. Returns `Ok(())` so callers `?` straight into their
/// OpResult. Errs (never silently no-ops) when the profile/OAuth block is
/// missing, the save fails, or the state flock can't be taken — callers must
/// refuse to act on the rotated pair in every case. Every persist-side failure
/// uses the same "failed to persist rotated tokens" message so the toast text is
/// identical regardless of leg (none reachable in practice — a profile selected
/// for rotation always has an OAuth block).
pub(crate) fn apply_rotated_tokens_locked(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    tok: TokenResponse,
) -> Result<()> {
    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    let mut cfg = config.lock().expect("config mutex poisoned");
    // Rotation coherence (#1): a rotation of the ACTIVE profile revokes the
    // single-use refresh token the macOS Keychain copy carries — the running
    // `claude` (which re-reads the Keychain per request) would sign out at
    // that stale token's expiry while every clauth copy stays green (observed
    // on-device 2026-07-07). The mirror DECISION and the creds snapshot are
    // made under the locked section below, so the written pair is exactly the
    // persisted one; the `/usr/bin/security` shell-out itself runs after the
    // flock is released (it can hang up to its 20 s kill deadline, and the
    // global state flock must never be held across a subprocess — before this
    // function the locked section contained only fast disk writes). In-process
    // switches stay excluded for the whole window by the config mutex held
    // across this function.
    #[cfg(target_os = "macos")]
    let mut mirror: Option<crate::profile::ClaudeCredentials> = None;
    with_state_lock(|held| {
        // The profile may have been deleted or renamed out-of-process since this
        // caller's config was loaded (the single-fetcher holds a stale config
        // between reloads). `save_profile` and the stage below would recreate its
        // directory, so ask the on-disk list — under the flock, the one stable
        // answer — before either write.
        if !crate::profile::is_configured(name).unwrap_or(false) {
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        }
        let Some(profile) = cfg.find_mut(name) else {
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        };
        // CLA-ROLL: the flag is adopted from DISK before the whole-profile
        // save below — the in-memory profile can predate a completed
        // `static-token --clear` (a separate process this snapshot never
        // sees), and `save_profile` persists the WHOLE profile, so the stale
        // flag would resurrect the disarm and the stamp below would re-create
        // the sidecar the operator was just told is gone. The read is stable
        // under the state flock and the RotationGuard every caller holds
        // across this function; an unreadable profile keeps the in-memory
        // flag, the same fallback as `rolling_install_gate`'s disk re-read.
        if let Ok(disk) = crate::profile::load_profile(name) {
            profile.rolling_token = disk.rolling_token;
        }
        let Some(creds) = profile.credentials_mut(held) else {
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        };
        let Some(oauth) = creds.claude_ai_oauth.as_mut() else {
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        };
        // Pre-rotation access token, kept for the Keychain-mirror gate below:
        // it tells "the live file is a stale mirror of OUR OWN chain" apart
        // from a genuinely foreign CC re-login.
        #[cfg(target_os = "macos")]
        let old_access = oauth.access_token.clone();
        write_token_fields(oauth, tok);
        // Stage the rotated pair durably before the structured save (see
        // `stage_rotated_credentials`): a failed save or crash is recovered on
        // next load rather than stranding a dead single-use refresh chain.
        if let Some(creds) = profile.credentials.as_ref() {
            let _ = stage_rotated_credentials(name, creds);
        }
        if save_profile(profile).is_err() {
            // Sidecar stays in place; load_profile adopts it on the next start.
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        }
        clear_staged_credentials(name);
        // CLA-ROLL: a rolling-token split profile re-stamps its session token
        // from the freshly rotated chain on EVERY rotation, active or parked —
        // a fast disk write inside the locked section, same durability class
        // as the credential save above. The pair itself still never leaves
        // clauth custody; only the (refresh-less) access token rolls forward.
        // An ABSENT sidecar is stamped too (it arms on the next rotation —
        // closes the race where a switch gate sees a comfortable chain before
        // any sidecar exists); only a NotLongLived mis-fill is left alone, so
        // the roll never destroys evidence of whatever wrote it.
        let stamp_sidecar = cfg.find(name).is_some_and(|p| p.rolling_token)
            && !matches!(
                crate::claude::session_token_status(name),
                Some(crate::claude::SessionTokenStatus::NotLongLived)
            );
        if stamp_sidecar
            && let Some(oauth) = cfg
                .find(name)
                .and_then(|p| p.credentials.as_ref())
                .and_then(|c| c.claude_ai_oauth.as_ref())
            && let Err(e) = crate::claude::stamp_rolling_token(name, oauth)
        {
            // Loud, non-fatal: the rotation is durable; the next rotation or
            // the switch-in gate retries the stamp. The stale rolling token keeps
            // serving until its real expiry, and every surface shows that
            // countdown honestly.
            logline!("clauth: rotated '{name}' but re-stamping session-token.json failed: {e:#}");
        }
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() && cfg.is_active(name) {
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
                    && let Ok(creds) =
                        crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(&path)
                    && creds.refresh_token().is_none()
                {
                    mirror = Some(creds);
                }
            } else if live_login_is_foreign(name, &old_access) {
                logline!(
                    "clauth: rotated '{name}' but the live login diverged (a re-login clauth \
                     doesn't own). Keychain left untouched; {}",
                    crate::format::RESOLVE_IN_TUI
                );
            } else if cfg.find(name).is_some_and(|p| p.rolling_token)
                && crate::claude::session_token_status(name).is_none()
            {
                // CLA-ROLL: flag on but NO sidecar right now — the arming stamp
                // write just failed (logged above). Never ship the rotating
                // pair to the Keychain for a rolling-token profile; the
                // previous rolling bearer keeps serving until the roll heals
                // (next rotation, the switch gate, or a `clauth rolling-token` re-arm).
                // A NotLongLived mis-fill deliberately does NOT take this
                // branch: a disengaged split behaves as vanilla (the pair
                // mirror below is what keeps CC alive there).
            } else {
                mirror = cfg.find(name).and_then(|p| p.credentials.as_ref()).cloned();
            }
        }
        Ok(())
    })?;
    // A failed state flock surfaces as the `Err` from `with_state_lock` above,
    // so a poisoned/unavailable lock never looks like a successful rotation.
    // A mirror failure is loud but non-fatal: the rotation itself is durable,
    // and the next rotation or switch retries the write.
    #[cfg(target_os = "macos")]
    if let Some(creds) = mirror
        && let Err(e) = crate::keychain::keychain_mirror_rotation(&creds)
    {
        logline!(
            "clauth: rotated '{name}' but the Keychain mirror failed: {e:#}. A \
             running claude signs out when its old token expires; run `clauth {name}` \
             to reinstall"
        );
    }
    Ok(())
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
        // The live slot reads healthy again: whatever refusal state stood is
        // resolved. Drop the once-per-state record so a FUTURE standing
        // refusal — the same reason included — is news again.
        remove_profile_cache(name, ADOPT_REFUSAL_FILE);
        return None;
    }
    let Ok(Some(live)) = crate::claude::read_claude_credentials() else {
        return None;
    };
    let live_oauth = live.claude_ai_oauth.as_ref()?;
    live_oauth.refresh_token.as_ref()?;
    let (Some(live_expires), Some(stored_expires)) = (live_oauth.expires_at, stored_expires) else {
        return None;
    };
    if live_expires <= stored_expires {
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
        // it cannot land while this leg holds it). `save_profile` would recreate
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
        save_profile(profile)?;
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
    write_profile_cache(name, ACCOUNT_ID_CACHE_FILE, &live_id);
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
    // CLA-ROLL: rolling-token profiles own their entire install story in
    // [`rolling_install_gate`] — every sidecar state (fresh/stale/absent/
    // mis-filled/dead-chain) is decided there, so no rolling-token profile can fall
    // through a vanilla path and install the shared rotating pair.
    if profile_rolling_token(config, name) {
        return rolling_install_gate(config, name, refresher, AUTH_GATE_GRACE_MS, LockWait::Block);
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

/// How [`rolling_install_gate`] takes the profile's rotation lock.
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
}

/// CLA-ROLL: the complete install gate for a rolling-token profile. Every
/// sidecar state is decided here:
///
///   * mis-filled sidecar → healed (evidence quarantined, static mint
///     restored) when a LIVE backup exists; a repair that raced ahead of us
///     rejoins the normal table below. With nothing live to restore the
///     split stays disengaged (loud), and what happens next follows `wait`:
///     the Block paths install through the SAME plain gate a non-rolling
///     mis-fill takes — never silently — while the NoWait leg answers
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
                    LockWait::Block => vanilla_install_gate(config, name, refresher),
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
    // On the Block path, `acquire` BLOCKS on the flock, so its error arm is a
    // filesystem or permissions problem under `~/.clauth` and never contention
    // — the same correction upstream made to `Cause::RotationLockUnavailable`.
    // On the NoWait path a held lock IS contention, and gets its own cause.
    let guard = match wait {
        LockWait::Block => match RotationGuard::acquire(name) {
            Ok(guard) => guard,
            Err(_) => return AuthGate::Transient(rotation_lock_unavailable(name)),
        },
        LockWait::NoWait => match RotationGuard::try_acquire(name) {
            Ok(Some(guard)) => guard,
            Ok(None) => return AuthGate::Transient(rotation_lock_held(name)),
            Err(_) => return AuthGate::Transient(rotation_lock_unavailable(name)),
        },
    };
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
            LockWait::Block => {
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
    // sidecar existed, so `install_source_path` handed it credentials.json and
    // spending the refresh here revokes the chain under it. Asked through
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
/// reach the macOS Keychain (a `Refreshed` outcome already mirrored through
/// the rotation hook; the running `claude` re-reads the Keychain per
/// request) — same refresh-less content belt as the hook: nothing carrying a
/// refresh token can ship through the rolling path.
pub(crate) fn restamp_rolling_token(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
) -> AuthGate {
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
        && let Err(e) = crate::keychain::keychain_mirror_rotation(&creds)
    {
        logline!("clauth: re-stamped '{name}' but the Keychain mirror failed: {e:#}");
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
fn adopt_disk_rotation(
    config: &crate::profile::ConfigHandle,
    name: &ProfileName,
    _guard: &RotationGuard,
) -> Result<()> {
    let Ok(disk) = crate::profile::load_profile(name) else {
        return Ok(());
    };
    if disk.refresh_token().is_none() {
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
/// a sibling process can hold that flock across the `/usr/bin/security`
/// shell-out for up to 20 seconds — a slow Keychain in ANOTHER process
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

    match refresher(&rt, scopes.as_deref()) {
        Ok(tok) => {
            if apply_rotated_tokens_locked(config, name, tok).is_err() {
                return AuthGate::Transient(crate::format::Transient::new(
                    crate::format::Cause::PersistFailed(name.to_string()),
                    crate::format::Retry::Wait,
                ));
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
                RefreshError::Invalid(_) => {
                    mark_auth_broken(config, name, true);
                    AuthGate::Broken
                }
                RefreshError::Transient(f) => AuthGate::Transient(f.as_refresh_transient()),
            }
        }
    }
}

/// Set or clear a profile's persisted `auth_broken` flag and save. Best-effort:
/// a failed save leaves the in-memory flag as set for this run (re-applied on the
/// next attempt). Locks `config` (outer) then the state flock (inner) — the
/// established save order.
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
    if !cfg.set_auth_broken(name, broken) {
        return;
    }
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
    let _ = crate::profile::set_auth_broken_persisted(name, broken);
}

#[cfg(test)]
#[path = "../tests/inline/oauth.rs"]
mod tests;
