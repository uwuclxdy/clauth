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
//!
//! The PKCE material, percent-codecs, and the loopback callback server live in
//! [`crate::loopback`] (CDX-3 R4 extraction — the codex browser login shares
//! them); this module keeps only the claude-specific wire shape. The re-exports
//! and thin wrappers below preserve this module's original surface so its
//! inline tests pin the extraction as behavior-identical.

use std::net::TcpListener;
#[cfg(test)]
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::Result;
use sha2::{Digest, Sha256};

// Test-only re-exports: the inline tests predate the `loopback` extraction
// and import these through this module — keeping that surface intact is what
// pins the extraction as behavior-identical.
use crate::loopback::{BindPort, bind_loopback, new_pkce, percent_encode, random_b64url};
#[cfg(test)]
pub(crate) use crate::loopback::{
    base64url_nopad, challenge_from_verifier, percent_decode, query_param, request_target,
};
use crate::profile::{ClaudeCredentials, OAuthToken};
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

/// The claude flow's callback path (ephemeral-port loopback redirect).
const CALLBACK_PATH: &str = "/callback";

/// Build the authorize URL. `code=true` is appended unconditionally, exactly as
/// the Claude Code binary does for every authorize request (loopback and manual
/// alike) — it selects the CLI code flow; the loopback redirect still fires.
fn authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?code=true&client_id={cid}&response_type=code&redirect_uri={ru}\
         &scope={scope}&code_challenge={cc}&code_challenge_method=S256&state={state}",
        cid = percent_encode(crate::oauth::CLIENT_ID),
        ru = percent_encode(redirect_uri),
        // Claude Code form-encodes the scope separators as `+`, not `%20`
        // (`docs/wire-parity.md`); every scope token is itself unreserved-safe
        // apart from its colons, which `percent_encode` still renders as `%3A`.
        scope = percent_encode(SCOPES).replace("%20", "+"),
        cc = percent_encode(challenge),
        state = percent_encode(state),
    )
}

/// [`crate::loopback::handle_callback`] pinned to the claude callback path —
/// kept as a module-local name so the inline tests exercise the extraction
/// through this module's original surface. (Production reaches it through
/// [`wait_for_code`]; only the tests call it directly.)
#[cfg(test)]
fn handle_callback(stream: TcpStream, expected_state: &str) -> Result<Option<String>> {
    crate::loopback::handle_callback(stream, expected_state, CALLBACK_PATH)
}

/// [`crate::loopback::wait_for_code`] pinned to the claude callback path.
fn wait_for_code(
    listener: &TcpListener,
    expected_state: &str,
    deadline: Instant,
) -> Result<String> {
    crate::loopback::wait_for_code(listener, expected_state, deadline, CALLBACK_PATH)
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
    pub(crate) account_uuid: Option<String>,
}

/// Run the full interactive login: open the browser, catch the loopback
/// redirect, exchange the code, and return a completed [`LoginOutcome`].
/// `progress` receives [`LoginProgress`] milestones — the `AuthorizeUrl` event
/// fires just before opening the browser (the CLI prints it so the flow is
/// observable and the URL can be pasted if the browser doesn't open; the TUI
/// also renders the later stages). Opening the browser is best-effort: on
/// failure the announced URL is the fallback and the listener still waits.
/// Blocks the caller for the browser round-trip (up to [`LOGIN_TIMEOUT_SECS`]).
pub(crate) fn login_with(progress: impl Fn(LoginProgress<'_>)) -> Result<LoginOutcome> {
    let (verifier, challenge) = new_pkce()?;
    let state = random_b64url(32)?;

    let (listener, port) = bind_loopback(BindPort::Ephemeral)?;
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
    let url = authorize_url(&redirect_uri, &challenge, &state);

    progress(LoginProgress::AuthorizeUrl(&url));
    let _ = crate::platform::open_url(&url);

    let deadline = Instant::now() + Duration::from_secs(LOGIN_TIMEOUT_SECS);
    let code = wait_for_code(&listener, &state, deadline)?;
    progress(LoginProgress::ExchangingCode);
    let token = crate::oauth::exchange_code(&code, &verifier, &redirect_uri, &state)?;
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
        Some(sub) => format!(
            "  plan: {} (token verified against the API)",
            crate::usage::PlanTier::from_subscription_type(Some(sub)).display()
        ),
        None => "  plan: will populate on the first usage refresh".to_string(),
    };
    format!(
        "  refresh sha256: {sha}…\n  scopes: {scopes}\n  access token expires: {expiry}\n{plan}"
    )
}

#[cfg(test)]
#[path = "../tests/inline/oauth_login.rs"]
mod tests;
