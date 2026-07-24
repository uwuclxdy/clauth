use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::claude::{LinkState, classify_credentials_link};
use crate::lock::with_state_lock;
use crate::logline::logline;
use crate::profile::{
    AppConfig, OAuthToken, clear_staged_credentials, save_profile, stage_rotated_credentials,
};
use crate::runtime::{RotationGuard, has_live_session};
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
/// the wire, `docs/wire-parity.md`). A profile's stored `scopes` array is often
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

/// The `anthropic-beta` set Claude Code sends on `/v1/messages` — its full
/// feature list, distinct from the single `oauth-2025-04-20` on `/usage`.
/// Captured 2026-07-14 against CC 2.1.209; drifts with CC's bundle, re-capture
/// on a bump (`docs/wire-parity.md`).
const KICK_ANTHROPIC_BETA: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05";

/// anthropic-sdk-js (stainless) version CC 2.1.209 bundles, sent verbatim on the
/// kick so its client-instrumentation headers match CC's. NOTE: this is a
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

/// Build a safe error for a **2xx** token-endpoint body that failed to
/// deserialize into [`TokenResponse`] (TECH-9 #14). Such a body still contains the
/// live access+refresh tokens, so it must NEVER be interpolated into an error that
/// surfaces on `clauth login`, a TUI rotate toast, `status.json`, or `daemon.log` —
/// a token pasted into a public issue is account takeover.
///
/// The serde `Display` is deliberately NOT used: an `invalid type`/`invalid value`
/// error echoes the offending scalar, which for an unexpected body shape could be a
/// token substring. Instead report only the error *category* + line/column, the HTTP
/// status, and the body length — a value-free channel that still pinpoints the
/// failure. The raw body is withheld entirely.
// pub(crate): the codex refresh (`codex::oauth`) shares this discipline — a
// 2xx token body that fails to parse still holds live credentials on either
// harness, and one implementation keeps the withholding rule single-sourced.
pub(crate) fn token_parse_error(
    e: serde_json::Error,
    status: u16,
    body_len: usize,
) -> anyhow::Error {
    let kind = match e.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "malformed json",
        serde_json::error::Category::Data => "unexpected shape",
        serde_json::error::Category::Eof => "truncated",
    };
    anyhow::anyhow!(
        "token endpoint returned HTTP {status} but its body did not parse as a token \
         response ({kind} at line {}, column {}); {body_len} bytes withheld \
         (contains live credentials)",
        e.line(),
        e.column(),
    )
}

// pub(crate): one HTTP agent (connection pool + timeouts +
// http_status_as_error(false)) serves both harnesses' token endpoints —
// `codex::oauth` posts to auth.openai.com through the same client.
pub(crate) static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        // ureq 3 defaults non-2xx to `Err(Error::StatusCode)`, which `kick`'s
        // error mapping collapsed into `KickError::Other` — making the
        // 401 → rotate-and-retry leg unreachable. With the flag off, `kick`
        // reads the status from the `Ok` response and `refresh` checks it
        // explicitly below.
        .http_status_as_error(false)
        .build()
        .into()
});

/// The CDX-5 proxy's upstream agent — the token AGENT's config plus a global
/// timeout so a blackholed upstream that sends the response head then goes
/// TCP-silent mid-SSE-stream can't park a connection thread forever (review
/// MED: the streaming body read is otherwise unbounded).
///
/// The global timeout is a LEAK BACKSTOP, not turn-end detection: ureq's
/// `timeout_global` fires even while bytes are actively flowing (pinned by
/// `ureq_global_timeout_truncates_an_actively_streaming_body`), so any value
/// a live stream can reach truncates that turn mid-flight. Turn-end comes
/// from the relay's terminal-event sniffer (`proxy::sse`); this ceiling only
/// bounds a genuinely wedged connection, and must stay far above any
/// legitimate single-request stream.
///
/// `timeout_recv_response` is deliberately ABSENT: in ureq 3 that deadline
/// keeps running through the BODY read, not just the headers (pinned by
/// `ureq_recv_response_timeout_kills_the_streaming_body`). The 2026-07-18
/// incident's actual assassin was a 30 s value here — every model turn whose
/// stream outlived 30 s died as `TRUNCATED … timeout: receive response`,
/// which codex reports as "stream closed before response.completed" and
/// replays from scratch (the first-pass diagnosis blamed the then-15-minute
/// global; the timestamped relay summaries this incident added showed the
/// kills at 29 s). SSE headers arrive immediately on a 200, so an
/// headers-only bound has nothing real to protect anyway — connect +
/// global cover the wedge cases.
pub(crate) static PROXY_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(2 * 60 * 60)))
        .http_status_as_error(false)
        .build()
        .into()
});

/// Cap a raw HTTP error body to its first line, max 200 chars, before it
/// reaches a user-facing toast — an upstream error page must not flood a
/// one-line surface.
// pub(crate) for the same single-source reason as `token_parse_error`.
pub(crate) fn http_error(status: u16, body: &str) -> anyhow::Error {
    let detail: String = body
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    if detail.is_empty() {
        anyhow::anyhow!("HTTP {status}")
    } else {
        anyhow::anyhow!("HTTP {status}: {detail}")
    }
}

/// A token-refresh failure, split so the AUTH-1 gate can tell a *permanently*
/// revoked/invalid refresh token (quarantine the account — `clauth login` is the
/// only fix) from a *transient* network/429/5xx blip (refuse this one switch,
/// retry next tick — never quarantine a healthy account on a hiccup).
pub(crate) enum RefreshError {
    /// The endpoint confirmed the refresh token itself is dead — quarantine the
    /// account (`clauth login` is the only fix). See
    /// [`refresh_rejection_is_terminal`] for the status/body split.
    Invalid(String),
    /// The refresh token may still be good: a transport failure, 429, 5xx, or a
    /// rejection the endpoint did not confirm as `invalid_grant`. Retry; never
    /// quarantine.
    Transient(anyhow::Error),
}

impl From<RefreshError> for anyhow::Error {
    fn from(e: RefreshError) -> Self {
        match e {
            RefreshError::Invalid(msg) => anyhow::anyhow!(msg),
            RefreshError::Transient(e) => e,
        }
    }
}

/// The refresh request body CC's axios client posts to the token endpoint.
/// Pure so the exact wire JSON (field set + canonical `scope` order) is
/// golden-tested against the captured CC shape (`docs/wire-parity.md`).
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
    let body =
        refresh_body(refresh_token, scopes).map_err(|e| RefreshError::Transient(e.into()))?;

    let mut response = AGENT
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Accept", TOKEN_ACCEPT)
        .header("User-Agent", TOKEN_USER_AGENT)
        .send(&body)
        .map_err(|e| RefreshError::Transient(anyhow::Error::from(e)))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| RefreshError::Transient(anyhow::Error::from(e)))?;
    if refresh_rejection_is_terminal(status, &text) {
        return Err(RefreshError::Invalid(http_error(status, &text).to_string()));
    }
    if status >= 400 {
        return Err(RefreshError::Transient(http_error(status, &text)));
    }

    serde_json::from_str(&text)
        .map_err(|e| RefreshError::Transient(token_parse_error(e, status, text.len())))
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

pub(crate) fn refresh(refresh_token: &str, scopes: Option<&str>) -> Result<TokenResponse> {
    refresh_result(refresh_token, scopes).map_err(Into::into)
}

/// A profile's stored granted scopes, space-joined, for the refresh `scope`
/// field — read under the config lock and returned owned so no lock is held
/// across the HTTP refresh. `None` (→ [`REFRESH_SCOPES_FALLBACK`]) for an
/// unknown profile or one without stored scopes. Callers must not already hold
/// the config lock.
pub(crate) fn stored_scopes(config: &crate::profile::ConfigHandle, name: &str) -> Option<String> {
    config.lock().ok()?.find(name)?.scopes_joined()
}

/// Exchange an authorization code (from the interactive loopback login in
/// `oauth_login`) for an OAuth token pair. Uses the same client + HTTP agent as
/// [`refresh`], against [`TOKEN_ENDPOINT`] (the `platform.claude.com` host the
/// current Claude Code binary uses), carrying the same axios-mimicking headers.
/// `redirect_uri` MUST byte-match the one sent to the authorize endpoint, and
/// `state` echoes the value round-tripped through the browser.
pub(crate) fn exchange_code(
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<TokenResponse> {
    let body = exchange_body(code, code_verifier, redirect_uri, state)?;

    let mut response = AGENT
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Accept", TOKEN_ACCEPT)
        .header("User-Agent", TOKEN_USER_AGENT)
        .send(&body)
        .map_err(anyhow::Error::from)?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(anyhow::Error::from)?;
    if status >= 400 {
        return Err(http_error(status, &text));
    }

    serde_json::from_str(&text).map_err(|e| token_parse_error(e, status, text.len()))
}

/// A kick failure. Distinguishes a 401 (access token expired — rotate the chain
/// and retry) from every other failure (body encode, transport, or any non-401
/// HTTP status), which is terminal for this attempt. Mirrors `FetchError::Status`
/// so the auto-start rotation leg reacts to the same signal the fetch path does.
enum KickError {
    /// The Messages endpoint returned this >=400 status; a 429 carries the
    /// limiter's own metadata when the response held any.
    Status(u16, Option<KickRateLimit>),
    /// Body encode or transport failure before a status was seen.
    Other(anyhow::Error),
}

impl From<KickError> for anyhow::Error {
    fn from(e: KickError) -> Self {
        match e {
            KickError::Status(s, _) => anyhow::anyhow!("HTTP {s}"),
            KickError::Other(e) => e,
        }
    }
}

/// Human string for a kick failure, for the diagnostic `logline!` when a kick
/// dies on something the recovery paths don't handle (non-401/429 status,
/// transport, body encode). Pure so the mapping is unit-testable without HTTP.
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
/// advertised reset (2026-07-15, `docs/wire-parity.md`), so callers retry with
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
    kick_to(MESSAGES_ENDPOINT, access_token)
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
/// Same double-spend guards as `fetch_with_rotation`'s rotation leg:
/// `RotationGuard` outermost across the refresh HTTP window, a `has_live_session`
/// re-check under the guard (a live session refreshes the chain itself), and the
/// rotated pair returned to the caller for the live token snapshot. A first kick
/// that succeeds spends only the access token and takes no `RotationGuard`.
///
/// Each recovery step is paced by [`ROTATION_STEP_DELAY_MS`] (kick → rotate →
/// retry kick → caller's usage re-fetch); none of the sleeps holds the rotation
/// lock. `activity` (the scheduler's store) drives the spinner; the CLI passes
/// `None`.
pub(crate) fn auto_start_kick(
    config: &crate::profile::ConfigHandle,
    name: &str,
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
    if has_live_session(name) {
        return KickResult::not_opened_with(first_rl);
    }

    // Refresh spinner during the round trip, then back to Fetching for the retry
    // kick + the caller's fetch (the kick runs inside the scheduler's fetch leg).
    if let Some(activity) = activity {
        mark_activity(activity, name, ProfileActivity::Refreshing);
    }
    let refreshed = refresh(rt, stored_scopes(config, name).as_deref());
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
enum RotateOutcome {
    /// `RotationGuard::acquire` failed — a live session or sibling worker holds
    /// the per-profile rotation lock. No `OpResult` was emitted.
    GuardBusy,
    /// The HTTP/persist leg ran and emitted its `OpResult`. The bool is whether
    /// the rotated pair was persisted.
    Persisted(bool),
}

/// Body of each [`refresh_all`] worker. Holds the per-profile rotation lock
/// across the ENTIRE HTTP window so an external `clauth start <name>` cannot
/// begin a refresh of the same single-use token while ours is in flight (the
/// state flock can't — it must release across the round trip). Ordering rule
/// (matches `ProfileRuntime::acquire`): RotationGuard OUTERMOST, then state
/// flock inside. With the guard held, the `has_live_session` check below is
/// authoritative, not a TOCTOU probe: a session that won the race stamped its
/// PID file before releasing the guard; one that lost is blocked here until we
/// finish and persist.
///
/// A live session is ALWAYS skipped — never rotated, not even on a user-forced
/// rotate. It owns the single-use refresh chain and advances it itself, so our
/// stored token is stale; refreshing it would 400 ("refresh token not found or
/// invalid"). `force` (a rotate-all concern, see `rotation_candidates`) governs
/// only the diverged-active profile, never this safety skip.
///
/// HTTP/persist leg emits one `OpResult { kind: Refreshing }` and clears the
/// activity slot. Returns [`RotateOutcome::GuardBusy`] without emitting an
/// `OpResult` when the lock can't be acquired (slot never pre-stamped here;
/// `refresh_all` pre-stamps and clears it). No-refresh-token / skipped-live-
/// session legs return [`RotateOutcome::Persisted(false)`] silently (the live-
/// session case is messaged up front by the single-rotate caller).
fn rotate_one_inner(
    config: &crate::profile::ConfigHandle,
    name: &str,
    activity: Option<&ActivityStore>,
    sender: &OpResultSender,
) -> RotateOutcome {
    let Ok(_rotation_guard) = RotationGuard::acquire(name) else {
        return RotateOutcome::GuardBusy;
    };
    let token = {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let cfg = config.lock().expect("config mutex poisoned");
        with_state_lock(|| {
            // A live `clauth start` session owns this profile's single-use OAuth
            // chain and refreshes it itself, so our stored refresh token is
            // already spent — rotating it 400s ("refresh token not found or
            // invalid"). Never rotate a live session; this in-guard check is
            // authoritative (a session that won the race stamped its PID before
            // releasing the RotationGuard). Skipping returns Persisted(false).
            if has_live_session(name) {
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
    let outcome = refresh(&rt, scopes.as_deref())
        .and_then(|tok| apply_rotated_tokens_locked(config, name, tok));
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
/// Diverged-active profiles are included only when `force`; live-session
/// profiles are ALWAYS excluded (a running session owns the single-use chain,
/// so our stored token is stale — rotating it 400s, `force` or not).
pub(crate) fn rotation_candidates(config: &AppConfig, force: bool) -> Vec<(String, String)> {
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
            // Never rotate a profile with a live `clauth start` session — its
            // chain is owned and advanced by that session; force does not apply.
            if has_live_session(&p.name) {
                return None;
            }
            Some((p.name.to_string(), p.refresh_token()?.to_string()))
        })
        .collect()
}

/// Refreshes every profile's OAuth token pair (rotated pair saved to disk).
/// Mirrors what Claude Code does silently on launch — minus the kick.
///
/// Profiles without a stored refresh token are skipped, as are profiles with a
/// live `clauth start` session (always — they own their own chain). Network/
/// revocation failures are swallowed per-profile; cached state stays put.
/// `force` bypasses only the diverged-active guard.
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
    let handles: Vec<(String, _)> = snapshots
        .into_iter()
        .map(|(name, _rt)| {
            let config = Arc::clone(config);
            let activity = Arc::clone(activity);
            let sender = sender.clone();
            let name_for_handle = name.clone();
            let h = std::thread::spawn(move || {
                // Holds the per-profile RotationGuard across the HTTP window so
                // an external `clauth start <name>` cannot double-spend this
                // single-use token mid-rotation. A session that started after
                // `rotation_candidates` snapshotted is caught by the in-guard
                // `has_live_session` skip inside (returns Persisted(false)).
                let outcome = rotate_one_inner(&config, &name, Some(&activity), &sender);
                (name, outcome)
            });
            (name_for_handle, h)
        })
        .collect();

    let mut refreshed = Vec::new();
    for (name, h) in handles {
        match h.join() {
            Ok((n, RotateOutcome::Persisted(true))) => refreshed.push(n),
            // Guard-fail leg never emits an OpResult, so this pre-stamped slot
            // would freeze the spinner AND swallow the failure. Emit the Danger
            // toast (matches the pre-collapse worker) and clear.
            Ok((n, RotateOutcome::GuardBusy)) => {
                let _ = sender.send(OpResult {
                    name: n.clone(),
                    outcome: Err(anyhow::anyhow!("failed to acquire rotation lock")),
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
/// Same discipline: `rotate_one_inner` holds the per-profile RotationGuard across
/// the HTTP window with a `has_live_session` skip, so a profile with a live
/// `clauth start` session is never rotated (its stored token is stale — the
/// caller refuses up front; this is the backstop). On success the profile is
/// pushed onto `refetch` so the next tick re-fetches its usage. Returns `true`
/// when a new pair persisted.
pub(crate) fn rotate_one(
    config: &crate::profile::ConfigHandle,
    name: &str,
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
        // as refresh_all's join loop does for a busy guard.
        RotateOutcome::GuardBusy => {
            let _ = sender.send(OpResult {
                name: name.to_string(),
                outcome: Err(anyhow::anyhow!("failed to acquire rotation lock")),
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
pub(crate) fn prime_window(config: &crate::profile::ConfigHandle, name: &str) -> bool {
    let (access_token, refresh_token, expires_at) = {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let cfg = config.lock().expect("config mutex poisoned");
        match with_state_lock(|| {
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
    name: &str,
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
    with_state_lock(|| {
        let Some(profile) = cfg.find_mut(name) else {
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        };
        let Some(creds) = profile.credentials.as_mut() else {
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
        // CLA-FEED: a feed-enabled split profile re-stamps its session token
        // from the freshly rotated chain on EVERY rotation, active or parked —
        // a fast disk write inside the locked section, same durability class
        // as the credential save above. The pair itself still never leaves
        // clauth custody; only the (refresh-less) access token is fed forward.
        // An ABSENT sidecar is fed too (arms the feed on the next rotation —
        // closes the race where a switch gate sees a comfortable chain before
        // any sidecar exists); only a NotLongLived mis-fill is left alone, so
        // the feed never destroys evidence of whatever wrote it.
        let feed_sidecar = cfg.find(name).is_some_and(|p| p.session_feed)
            && !matches!(
                crate::claude::session_token_status(name),
                Some(crate::claude::SessionTokenStatus::NotLongLived)
            );
        if feed_sidecar
            && let Some(oauth) = cfg
                .find(name)
                .and_then(|p| p.credentials.as_ref())
                .and_then(|c| c.claude_ai_oauth.as_ref())
            && let Err(e) = crate::claude::feed_session_token(name, oauth)
        {
            // Loud, non-fatal: the rotation is durable; the next rotation or
            // the switch-in gate retries the feed. The stale fed token keeps
            // serving until its real expiry, and every surface shows that
            // countdown honestly.
            logline!("clauth: rotated '{name}' but feeding session-token.json failed: {e:#}");
        }
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() && cfg.is_active(name) {
            if crate::claude::has_session_token(name) {
                // CLA-SPLIT: the live slot intentionally holds this profile's
                // static session token — the rotated pair is the clauth-private
                // USAGE chain and must never be mirrored over it. Quiet: this
                // is the designed steady state, not a divergence.
                // CLA-FEED: what DOES ship to the Keychain for a feed-enabled
                // profile is the freshly FED sidecar (refresh-less bearer) —
                // the running claude re-reads the Keychain per request, so
                // this is exactly how the new token reaches live sessions.
                // The refresh-none re-check is a content-level belt: whatever
                // reaches the Keychain through the feed path can never carry
                // a refresh token (invariant #1 — a rotating pair in front of
                // sessions is the death the split exists to prevent).
                if feed_sidecar
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
            } else if cfg.find(name).is_some_and(|p| p.session_feed)
                && crate::claude::session_token_status(name).is_none()
            {
                // CLA-FEED: flag on but NO sidecar right now — the arming feed
                // write just failed (logged above). Never ship the rotating
                // pair to the Keychain for a feed-intended profile; the
                // previous fed bearer keeps serving until the feed heals
                // (next rotation, the switch gate, or a `clauth feed` re-arm).
                // A NotLongLived mis-fill deliberately does NOT take this
                // branch: a disengaged split behaves as vanilla (the pair
                // mirror below is what keeps CC alive there — CLA-SPLIT-3).
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
        && let Err(e) = crate::keychain::keychain_write(&creds)
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
/// (rotation coherence, the future-proof half — #24 review). The running
/// `claude` and clauth hold ONE single-use refresh family; whoever refreshes
/// first revokes the other. Rather than racing, concede: CC maintains
/// `~/.claude/.credentials.json` as a regular-file mirror of its Keychain
/// login (rewritten at least on every CC launch), which is a prompt-free read
/// path to CC's current pair. When that mirror holds a FRESHER pair for the
/// SAME account, adopt it into the profile store — no refresh spent, any
/// stale `auth_broken` quarantine cleared (the account was never dead, we
/// just lost the race) — so clauth stays correct whatever refresh schedule a
/// future Claude Code ships.
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
/// token, and falsely quarantine the very account the adopt just saved.
///
/// `_rotation_guard` is proof the caller holds this profile's per-profile
/// rotation lock: the adopt mutates the same stored credential fields as a
/// refresh persist (`rotate_one_inner`), so both writers must serialize on
/// the same [`crate::runtime::RotationGuard`], not just the state flock.
/// Taken by reference because the flock is not reentrant — the refresh-failure
/// call site already holds the guard when it retries the adopt.
/// Once-per-login memo for the adopt refusal logs: a refused live login is
/// re-examined every poll, which used to re-emit the same two lines hundreds
/// of times an evening (observed 2026-07-11). Keyed by the refused access
/// token's hash — a NEW login re-arms the log; process-wide is correct
/// because every poller in this process refuses the same login for the same
/// reason. Returns whether this token has NOT been logged about yet.
fn first_refusal_of(access_token: &str) -> bool {
    use std::hash::{DefaultHasher, Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_REFUSED: AtomicU64 = AtomicU64::new(0);
    let mut hasher = DefaultHasher::new();
    access_token.hash(&mut hasher);
    let fp = hasher.finish().max(1); // 0 is the "nothing yet" sentinel
    LAST_REFUSED.swap(fp, Ordering::Relaxed) != fp
}

pub(crate) fn try_adopt_live_rotation(
    config: &crate::profile::ConfigHandle,
    name: &str,
    _rotation_guard: &crate::runtime::RotationGuard,
    identity: &dyn Fn(&str) -> Option<String>,
) -> Option<(String, Option<String>)> {
    use crate::profile_cache::{ACCOUNT_ID_CACHE_FILE, load_profile_cache, write_profile_cache};

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
    let expected: Option<String> = load_profile_cache::<String>(name, ACCOUNT_ID_CACHE_FILE)
        .or_else(|| {
            let alive = (now_ms() as i64) < stored_expires;
            match (&stored_access, alive) {
                (Some(tok), true) => identity(tok),
                _ => None,
            }
        });
    let Some(expected) = expected else {
        if first_refusal_of(&live_oauth.access_token) {
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
        if first_refusal_of(&live_oauth.access_token) {
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
    let adopted = with_state_lock(|| {
        if !cfg.is_active(name) {
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
        profile.credentials = Some(live.clone());
        save_profile(profile)?;
        if cfg.set_auth_broken(name, false) {
            logline!("clauth: '{name}' re-adopted from the live session — auth_broken cleared");
        }
        let _ = crate::profile::save_app_state(&cfg.state);
        Ok::<bool, anyhow::Error>(true)
    })
    .unwrap_or(false);
    if !adopted {
        return None;
    }
    // The adopted pair proves the chain is alive, so a standing `auth_broken`
    // is stale — the flag was set while CC held the fresher pair. Same lift as
    // the scheduler's `carry_external_rotation` (inlined here because the
    // config guard is already held); without it, an active recovered by a
    // CC-side re-login stays excluded from the fallback walk and refused as a
    // switch target until a manual `clauth login`.
    if cfg.set_auth_broken(name, false) {
        logline!("clauth: '{name}' re-authenticated: auth_broken cleared");
        let _ = crate::profile::save_app_state(&cfg.state);
    }
    write_profile_cache(name, ACCOUNT_ID_CACHE_FILE, &live_id);
    logline!(
        "clauth: adopted the live session's rotated login for '{name}' \
         (the running claude refreshed first, so no token spent)"
    );
    Some((
        live_oauth.access_token.clone(),
        live_oauth.refresh_token.clone(),
    ))
}

/// Whether the live `.credentials.json` holds a login clauth does NOT own —
/// i.e. genuinely [`LinkState::Diverged`] and not merely a stale regular-file
/// mirror of this profile's own pre-rotation pair. On macOS Claude Code
/// rewrites the live file as a regular-file copy of the Keychain, so the
/// moment a rotation lands, `classify_credentials_link` reports Diverged
/// against the NEW stored token even though the live login is still our own
/// chain one step behind — that stale-mirror case must still be mirrored, or
/// the coherence write would skip exactly when it matters. Only a live token
/// matching NEITHER the new nor the pre-rotation pair is foreign (a real CC
/// re-login); an unreadable/unclassifiable state is treated as foreign so a
/// state we cannot understand is never overwritten.
#[cfg(target_os = "macos")]
fn live_login_is_foreign(name: &str, old_access: &str) -> bool {
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
    config.state.active_profile.as_deref().is_some_and(|name| {
        matches!(
            classify_credentials_link(name).ok(),
            Some(LinkState::Diverged)
        )
    })
}

/// Grace window (ms): a token with less than this much life left is treated as
/// expiring, so the AUTH-1 gate refreshes it *before* install rather than
/// letting the freshly-switched session hit a 401.
const AUTH_GATE_GRACE_MS: i64 = 60_000;

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
    /// A transient failure (network/429/5xx, a busy rotation lock, or a poisoned
    /// mutex) blocked a needed refresh. Do not install now; retry on a later
    /// tick. The account is NOT quarantined.
    Transient(anyhow::Error),
}

/// Pre-install auth gate (AUTH-1 / Incident C). Before a switch installs `name`'s
/// stored credentials into the macOS Keychain — which instantly re-authenticates
/// every running `claude` on this machine — make sure the token is live:
///   * third-party (api-key) profiles bypass the gate;
///   * an OAuth access token with more than [`AUTH_GATE_GRACE_MS`] of life
///     installs as-is;
///   * an expiring/expired token is refreshed through `refresher` and the rotated
///     pair persisted before install;
///   * a revoked/invalid refresh token quarantines the profile (`auth_broken`,
///     [`AuthGate::Broken`]) and refuses the switch.
///
/// `refresher` is injected so the gate is unit-testable offline (real callers
/// pass [`refresh_result`]; tests pass a fixture). The config mutex is never held
/// across the HTTP refresh, and the per-profile `RotationGuard` wraps the refresh
/// so a live session or sibling worker cannot double-spend the single-use token.
pub(crate) fn ensure_installable(
    config: &crate::profile::ConfigHandle,
    name: &str,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
) -> AuthGate {
    // CLA-FEED: feed-enabled profiles own their entire install story in
    // [`feed_install_gate`] — every sidecar state (fresh/stale/absent/
    // mis-filled/dead-chain) is decided there, so no feed profile can fall
    // through a vanilla path and install the shared rotating pair.
    if profile_session_feed(config, name) {
        return feed_install_gate(config, name, refresher, AUTH_GATE_GRACE_MS);
    }
    vanilla_install_gate(config, name, refresher)
}

/// The pre-CLA-FEED gate, byte-for-byte: static session-token profiles gate on
/// the token's clock; vanilla OAuth profiles refresh-if-expiring under the
/// rotation guard.
fn vanilla_install_gate(
    config: &crate::profile::ConfigHandle,
    name: &str,
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
            let clock_dead = expires_at.is_some_and(|exp| (now_ms() as i64) >= exp);
            if clock_dead {
                logline!(
                    "clauth: '{name}' long-lived token has expired — re-mint with \
                     `claude setup-token` (clauth login {name} --setup-token)"
                );
                return AuthGate::Broken;
            }
            return AuthGate::Ready;
        }
        // #53 review: the split engages only for a token that actually IS
        // long-lived. A sidecar holding a rotating pair is a mis-fill —
        // installing it would put a dies-in-hours token in front of sessions
        // with no refresher, so it is IGNORED (credentials.json installs
        // below, exactly as if the sidecar weren't there) and called out
        // here, the per-switch chokepoint, rather than on every hot-path stat.
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
    // acquired with no config lock held. A busy guard means a live session or
    // sibling worker is already on this chain — refuse this switch and retry.
    let Ok(guard) = RotationGuard::acquire(name) else {
        return AuthGate::Transient(anyhow::anyhow!(
            "'{name}' rotation lock busy; retry after the in-flight refresh"
        ));
    };
    // Authoritative under the guard: a live `clauth start` session owns and
    // advances this profile's single-use chain and keeps the Keychain fresh, so
    // refreshing here would 400 the session — install as-is.
    if has_live_session(name) {
        return AuthGate::Ready;
    }
    gate_under_guard(config, name, refresher, &guard, AUTH_GATE_GRACE_MS)
}

/// CLA-FEED: the complete install gate for a feed-enabled profile. Every
/// sidecar state is decided here (review round 1, findings #2/#5/#6):
///
///   * mis-filled sidecar → healed (evidence quarantined, static mint
///     restored) when a backup exists; without one the disengaged-vanilla
///     posture stands (loud) — but the pair is only ever installed through
///     the SAME plain gate a non-feed mis-fill takes, never silently;
///   * fresh fed token (or freshly restored mint) → install as-is, no locks;
///   * stale or absent → serialized under the profile's RotationGuard for the
///     whole read-and-restamp (a concurrent rotation's newer feed can no
///     longer be clobbered by an older cloned token), fed from the stored
///     chain when comfortable (no spend) or through the guarded refresh
///     (whose persist re-feeds via the rotation hook);
///   * live session on the ROTATING PAIR (started inside an arming window,
///     before any sidecar existed) → refuse to spend: refreshing would
///     revoke the chain under that session, the exact death the split
///     prevents. Sessions on a fed bearer never block — they hold nothing
///     refreshable;
///   * terminally dead chain → restore the static mint (Ready, degraded)
///     else Broken.
fn feed_install_gate(
    config: &crate::profile::ConfigHandle,
    name: &str,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
    fresh_horizon_ms: i64,
) -> AuthGate {
    use crate::claude::SessionTokenStatus;
    if matches!(
        crate::claude::session_token_status(name),
        Some(SessionTokenStatus::NotLongLived)
    ) {
        match crate::claude::heal_misfilled_sidecar(name) {
            Ok(true) => logline!(
                "clauth: '{name}' mis-filled sidecar quarantined; static mint restored \
                 (the feed re-arms it on the next rotation)"
            ),
            Ok(false) => {
                logline!(
                    "clauth: '{name}' sidecar is mis-filled and no static backup exists — \
                     the split stays disengaged (vanilla install); re-capture with \
                     `clauth login {name} --setup-token`"
                );
                return vanilla_install_gate(config, name, refresher);
            }
            Err(e) => return AuthGate::Transient(e),
        }
    }
    // Freshness is FED-shaped freshness: an hours-horizon LongLived token
    // with real life left. A static MINT (~1yr stamp) is deliberately NOT
    // "fresh" here — on a feed profile it is the live *fallback*, and the
    // gate's job is to supersede it with a Fable-capable fed bearer (the
    // deploy-day bug: the mint's far-future expiry read as fresh, so arming
    // never fed anything). A live mint also means the profile is ARMED
    // (`has_session_token` true), so every degrade path below can fall back
    // to Ready on it rather than deferring the switch.
    let now = now_ms() as i64;
    let fed_fresh = |st: Option<SessionTokenStatus>| {
        matches!(st, Some(SessionTokenStatus::LongLived(Some(e)))
            if !horizon_expiring(Some(e), false, fresh_horizon_ms)
                && e < now + crate::claude::MINT_HORIZON_MS)
    };
    let sidecar_live = |st: Option<SessionTokenStatus>| matches!(st, Some(SessionTokenStatus::LongLived(exp)) if !expiring(exp, false));
    if fed_fresh(crate::claude::session_token_status(name)) {
        return AuthGate::Ready;
    }
    // Mint-shaped, stale, or absent: everything below mutates the sidecar or
    // the chain, so it serializes with rotations on the cross-process guard.
    let Ok(guard) = RotationGuard::acquire(name) else {
        return AuthGate::Transient(anyhow::anyhow!(
            "'{name}' rotation lock busy; retry after the in-flight refresh"
        ));
    };
    // The rotation we just serialized with may have re-fed it already.
    if fed_fresh(crate::claude::session_token_status(name)) {
        return AuthGate::Ready;
    }
    match feed_from_stored_chain(config, name, &guard, fresh_horizon_ms) {
        FeedAttempt::Fed => return AuthGate::Ready,
        // A feed WRITE failure with a live mint still installs the mint
        // (degraded but serving — the next rotation retries the feed); with
        // no live sidecar it must not fall anywhere (the refresh leg would
        // early-Ready on the comfortable chain without feeding anything).
        FeedAttempt::WriteFailed(e) => {
            return if sidecar_live(crate::claude::session_token_status(name)) {
                logline!("clauth: '{name}' feed write failed ({e:#}); installing the mint");
                AuthGate::Ready
            } else {
                AuthGate::Transient(e)
            };
        }
        FeedAttempt::ChainStale => {}
    }
    // A live session with NO armed sidecar was started on the rotating pair
    // (install_source falls to credentials.json when has_session_token is
    // false) — spending the refresh here revokes the chain under it.
    if !crate::claude::has_session_token(name) && has_live_session(name) {
        return AuthGate::Transient(anyhow::anyhow!(
            "'{name}' has a live session holding the rotating chain (started before the \
             feed was armed) — restart that session or retry after it ends"
        ));
    }
    let gate = gate_under_guard(config, name, refresher, &guard, fresh_horizon_ms);
    match gate {
        // The refresh persisted through the rotation hook, which fed the
        // sidecar as a side effect.
        AuthGate::Refreshed => AuthGate::Refreshed,
        // A terminally dead usage chain degrades to the static mint —
        // restored from backup, or already sitting in the sidecar — instead
        // of benching an account whose sessions could still run.
        AuthGate::Broken => {
            if crate::claude::restore_static_mint(name).unwrap_or(false)
                || sidecar_live(crate::claude::session_token_status(name))
            {
                logline!(
                    "clauth: '{name}' usage chain is dead — sessions degrade to the static \
                     long-lived mint (`clauth login {name}` revives the chain and the feed)"
                );
                AuthGate::Ready
            } else {
                AuthGate::Broken
            }
        }
        // Transient chain trouble with a live mint: install the mint now
        // (degraded but serving) rather than deferring a switch a healthy
        // static token could carry; the feed self-heals on a later rotation.
        AuthGate::Transient(e) => {
            if sidecar_live(crate::claude::session_token_status(name)) {
                logline!(
                    "clauth: '{name}' chain refresh hit a transient failure ({e:#}); \
                     installing the mint while the feed retries"
                );
                AuthGate::Ready
            } else {
                AuthGate::Transient(e)
            }
        }
        // Ready from the vanilla leg = a sibling refreshed under the guard
        // window; its persist ran the feed hook.
        AuthGate::Ready => AuthGate::Ready,
    }
}

/// CLA-FEED: arm or re-stamp `name`'s fed sidecar right now — the CLI-enable
/// path. Same decision table as [`feed_install_gate`] (the CLI pre-clears a
/// mis-fill by quarantining it, so the vanilla fall-through leg is
/// unreachable here in practice — and if raced back in, `Ready` from the
/// vanilla gate still reports arming failure via the sidecar check).
pub(crate) fn arm_session_feed(
    config: &crate::profile::ConfigHandle,
    name: &str,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
) -> Result<()> {
    match feed_install_gate(config, name, refresher, AUTH_GATE_GRACE_MS) {
        AuthGate::Ready | AuthGate::Refreshed => {
            if crate::claude::has_session_token(name) {
                Ok(())
            } else {
                anyhow::bail!(
                    "'{name}' could not arm the fed sidecar (mis-filled sidecar without a \
                     backup?) — re-capture with `clauth login {name} --setup-token` or clear \
                     the sidecar, then re-run"
                )
            }
        }
        AuthGate::Broken => {
            anyhow::bail!("'{name}' usage chain is dead — `clauth login {name}` first, then re-run")
        }
        AuthGate::Transient(e) => Err(e),
    }
}

/// EXP-1/F2: how much life a fed sidecar must keep before the daemon's
/// re-feed leg leaves it alone. Fed bearers die in hours (they clone the
/// usage chain's access-token expiry); re-feeding this far ahead keeps a
/// running session's bearer alive across daemon idle gaps, spent-window poll
/// parking, and machine sleep — the RC-C death was a sidecar quietly hitting
/// its ~7h clock while re-feeds waited on a rotation that never came.
pub(crate) const FEED_REFEED_HORIZON_MS: i64 = 2 * 60 * 60 * 1000;

/// EXP-1/F2 due predicate for the scheduler's re-feed leg: an armed,
/// exp-carrying sidecar inside [`FEED_REFEED_HORIZON_MS`] of death. Absent
/// sidecars (arming is switch/rotation work), exp-less claims, and
/// NotLongLived mis-fills (switch-time healing owns those) are all not-due —
/// the timer's single job is clock freshness of what the feed installed.
pub(crate) fn fed_sidecar_refeed_due(name: &str, now: i64) -> bool {
    matches!(
        crate::claude::session_token_status(name),
        Some(crate::claude::SessionTokenStatus::LongLived(Some(exp)))
            if exp <= now + FEED_REFEED_HORIZON_MS
    )
}

/// EXP-1/F2: the scheduler-leg re-feed for one feed-enabled profile — the
/// same complete decision table as the switch-in gate (no-spend re-stamp from
/// a comfortable chain / guarded refresh / mint degrade), but judged against
/// the generous [`FEED_REFEED_HORIZON_MS`] instead of the switch gate's
/// seconds-tight grace. For the ACTIVE profile a no-spend re-stamp must also
/// reach the macOS Keychain (a `Refreshed` outcome already mirrored through
/// the rotation hook; the running `claude` re-reads the Keychain per
/// request) — same refresh-less content belt as the hook: nothing carrying a
/// refresh token can ship through the feed path.
pub(crate) fn refeed_session_token(
    config: &crate::profile::ConfigHandle,
    name: &str,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
) -> AuthGate {
    let gate = feed_install_gate(config, name, refresher, FEED_REFEED_HORIZON_MS);
    // A fed-shaped sidecar now clear of the horizon = the no-spend re-stamp
    // just landed (the Refreshed and mint-degrade paths log at their source).
    if matches!(gate, AuthGate::Ready) {
        let now = now_ms() as i64;
        if matches!(
            crate::claude::session_token_status(name),
            Some(crate::claude::SessionTokenStatus::LongLived(Some(exp)))
                if exp > now + FEED_REFEED_HORIZON_MS
                    && exp < now + crate::claude::MINT_HORIZON_MS
        ) {
            logline!("clauth: re-fed '{name}' session token ahead of its expiry");
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
        && let Err(e) = crate::keychain::keychain_write(&creds)
    {
        logline!("clauth: re-fed '{name}' but the Keychain mirror failed: {e:#}");
    }
    gate
}

/// CLA-FEED: whether `name` has the session feed enabled. A poisoned config
/// mutex or unknown profile reads `false` — the static/vanilla gates apply.
fn profile_session_feed(config: &crate::profile::ConfigHandle, name: &str) -> bool {
    config
        .lock()
        .ok()
        .and_then(|c| c.find(name).map(|p| p.session_feed))
        .unwrap_or(false)
}

/// Outcome of [`feed_from_stored_chain`].
enum FeedAttempt {
    /// Sidecar re-stamped from the stored chain — no refresh spent.
    Fed,
    /// The stored chain is itself expiring/broken (or absent): the caller
    /// routes to the guarded refresh leg, whose persist re-feeds the sidecar
    /// via the rotation hook.
    ChainStale,
    /// The chain is healthy but the sidecar write failed — the caller must
    /// NOT fall through (the refresh leg would early-Ready on the comfortable
    /// chain without re-feeding a stale sidecar).
    WriteFailed(anyhow::Error),
}

/// CLA-FEED: re-stamp `name`'s sidecar from the STORED usage chain when its
/// access token is comfortably live — the no-spend path for a stale fed token
/// at switch time. A standing `auth_broken` routes to `ChainStale` (server-side
/// revocation kills the access token with the chain, so a comfortable clock
/// proves nothing there — same rationale as [`expiring`]'s flag override).
/// `_rotation_guard` witnesses the caller holding the profile's rotation lock
/// for the whole read-and-write: without it, a concurrent rotation's NEWER fed
/// token could be clobbered by this call's older cloned access token.
fn feed_from_stored_chain(
    config: &crate::profile::ConfigHandle,
    name: &str,
    _rotation_guard: &RotationGuard,
    fresh_horizon_ms: i64,
) -> FeedAttempt {
    let Ok(cfg) = config.lock() else {
        return FeedAttempt::ChainStale;
    };
    let flagged = cfg.is_auth_broken(name);
    let chain = cfg
        .find(name)
        .and_then(|p| p.credentials.as_ref())
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .cloned();
    drop(cfg);
    let Some(oauth) = chain else {
        return FeedAttempt::ChainStale;
    };
    if horizon_expiring(oauth.expires_at, flagged, fresh_horizon_ms) {
        return FeedAttempt::ChainStale;
    }
    match crate::claude::feed_session_token(name, &oauth) {
        Ok(()) => FeedAttempt::Fed,
        Err(e) => FeedAttempt::WriteFailed(e),
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
    name: &str,
) -> std::result::Result<(Option<i64>, Option<String>, Option<String>, bool), AuthGate> {
    let Ok(cfg) = config.lock() else {
        return Err(AuthGate::Transient(anyhow::anyhow!(
            "config mutex poisoned"
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
/// [`AUTH_GATE_GRACE_MS`]; the EXP-1/F2 re-feed leg passes
/// [`FEED_REFEED_HORIZON_MS`] so a fed bearer is renewed HOURS before its
/// clock death, not seconds — the margin that keeps a running session alive
/// across daemon idle gaps and machine sleep.
fn horizon_expiring(expires_at: Option<i64>, flagged: bool, horizon_ms: i64) -> bool {
    flagged || expires_at.is_some_and(|exp| (now_ms() as i64) + horizon_ms >= exp)
}

/// Reconcile the in-memory profile with the on-disk store; the `_guard` witness
/// proves the [`RotationGuard`] is held, which makes the disk read stable. A
/// cross-process peer (the daemon, a second clauth) rotates and persists under
/// this same flock, and a caller that loaded config from disk once (a CLI or MCP
/// switch) can hold a snapshot predating that write. Tokens are opaque and no
/// writer rewinds the store (see the scheduler's `fresher_disk_pair`), so a
/// stored refresh token that DIFFERS from the in-memory one proves someone
/// advanced the single-use chain: adopt the disk pair and lift a stale
/// quarantine — the chain is alive under someone else's advance (mirrors
/// `carry_external_rotation`; a wrong lift self-corrects when the carried pair's
/// own refresh 400s). Unreadable or tokenless disk state is a no-op: the
/// in-memory shape stays the best available truth.
fn adopt_disk_rotation(config: &crate::profile::ConfigHandle, name: &str, _guard: &RotationGuard) {
    let Ok(disk) = crate::profile::load_profile(name) else {
        return;
    };
    if disk.refresh_token().is_none() {
        return;
    }
    {
        let Ok(mut cfg) = config.lock() else {
            return;
        };
        let Some(profile) = cfg.find_mut(name) else {
            return;
        };
        if profile.refresh_token() == disk.refresh_token() {
            return;
        }
        profile.credentials = disk.credentials;
    }
    mark_auth_broken(config, name, false);
}

/// The refresh leg; the `guard` witness proves the [`RotationGuard`] is held.
/// First adopts a cross-process rotation from disk ([`adopt_disk_rotation`]),
/// then re-reads the auth shape UNDER the guard — between the pre-check and
/// guard acquisition a sibling rotation (in-process OR a persisting peer) may
/// have spent the single-use refresh token and written a new pair, and
/// refreshing from that stale snapshot would 400 and wrongly quarantine a
/// healthy login. This function takes no token arguments, so post-guard
/// decisions structurally cannot reuse pre-guard data.
fn gate_under_guard(
    config: &crate::profile::ConfigHandle,
    name: &str,
    refresher: impl Fn(&str, Option<&str>) -> std::result::Result<TokenResponse, RefreshError>,
    guard: &RotationGuard,
    fresh_horizon_ms: i64,
) -> AuthGate {
    adopt_disk_rotation(config, name, guard);
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
                return AuthGate::Transient(anyhow::anyhow!(
                    "refreshed '{name}' but failed to persist the rotated tokens"
                ));
            }
            // A successful refresh clears any prior quarantine.
            mark_auth_broken(config, name, false);
            AuthGate::Refreshed
        }
        Err(RefreshError::Invalid(_)) => {
            mark_auth_broken(config, name, true);
            AuthGate::Broken
        }
        Err(RefreshError::Transient(e)) => AuthGate::Transient(e),
    }
}

/// Set or clear a profile's persisted `auth_broken` flag and save. Best-effort:
/// a failed save leaves the in-memory flag as set for this run (re-applied on the
/// next attempt). Locks `config` (outer) then `update_app_state` takes the state
/// flock (inner) — the established save order.
pub(crate) fn mark_auth_broken(config: &crate::profile::ConfigHandle, name: &str, broken: bool) {
    if let Ok(mut cfg) = config.lock()
        && cfg.set_auth_broken(name, broken)
    {
        // Log the TRANSITION only (`set_auth_broken` returns false on a no-op), so a
        // dropped login leaves one line in daemon.log — the "why did it break?" answer
        // — instead of silently flipping a flag or spamming every tick.
        if broken {
            logline!(
                "clauth: {} (flagged auth_broken)",
                crate::format::login_expired(name).line()
            );
        } else {
            logline!("clauth: '{name}' re-authenticated: auth_broken cleared");
        }
        // Persist the ONE delta against the LATEST disk state instead of a
        // blind whole-state rewrite from this process's possibly-stale
        // snapshot (the TECH-7 lost-update surface): a `clauth login` reauth
        // in ANOTHER process clears this same list concurrently, and a
        // last-writer-wins full save from either side would clobber the
        // other's unrelated state changes.
        let name_owned = name.to_string();
        let _ = crate::profile::update_app_state(|s| {
            if broken {
                if !s.auth_broken.iter().any(|n| n.as_str() == name_owned) {
                    s.auth_broken.push(name_owned.as_str().into());
                }
            } else {
                s.auth_broken.retain(|n| n.as_str() != name_owned);
            }
        });
    }
}

#[cfg(test)]
#[path = "../tests/inline/oauth.rs"]
mod tests;
