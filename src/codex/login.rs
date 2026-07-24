//! CDX-3 R5: interactive browser PKCE login for a codex account, minting a
//! profile-store snapshot WITHOUT ever touching the live `~/.codex/auth.json`
//! (the differentiator vs capture — the live login and the codex active
//! marker stay put). Wire shape verified at openai/codex `9ff47868` /
//! codex-cli 0.144.5 (`login/src/server.rs`, `pkce.rs`):
//!
//! - authorize at `auth.openai.com/oauth/authorize`, S256 PKCE, loopback
//!   redirect on the REGISTERED ports 1455 (fallback 1457), path
//!   `/auth/callback`;
//! - code exchange at `/oauth/token` as **form-urlencoded** (the refresh in
//!   `codex::oauth` is JSON — the endpoint speaks both, per grant);
//! - an optional secondary token-exchange mints the `OPENAI_API_KEY` codex
//!   stores alongside the OAuth tokens — failure never fails the login
//!   (codex's own `.ok()` semantics).
//!
//! The loopback server/PKCE material is the shared [`crate::loopback`]
//! module (same state-mismatch CSRF stop as the claude flow).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::loopback::{
    BindPort, bind_loopback, new_pkce, percent_encode, random_b64url, wait_for_code,
};

use super::oauth::{CODEX_CLIENT_ID, CODEX_TOKEN_ENDPOINT};

const CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";

/// The scope set codex requests (verbatim from `server.rs` at HEAD).
const CODEX_SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// The OAuth client's registered loopback redirect ports, tried in order.
const CODEX_CALLBACK_PORTS: [u16; 2] = [1455, 1457];

const CODEX_CALLBACK_PATH: &str = "/auth/callback";

/// How long to wait for the browser round-trip before giving up.
const LOGIN_TIMEOUT_SECS: u64 = 180;

/// The code-exchange response — all three required (unlike the refresh).
#[derive(Deserialize)]
pub(crate) struct CodexTokenExchange {
    pub(crate) id_token: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
}

/// Build the authorize URL (`server.rs` param set: PKCE S256 +
/// `id_token_add_organizations=true` + `codex_cli_simplified_flow=true` +
/// `originator`). Pure — golden-tested.
pub(crate) fn authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    format!(
        "{CODEX_AUTHORIZE_URL}?response_type=code&client_id={cid}&redirect_uri={ru}\
         &scope={scope}&code_challenge={cc}&code_challenge_method=S256\
         &id_token_add_organizations=true&codex_cli_simplified_flow=true\
         &originator=codex_cli_rs&state={state}",
        cid = percent_encode(CODEX_CLIENT_ID),
        ru = percent_encode(redirect_uri),
        scope = percent_encode(CODEX_SCOPES).replace("%20", "+"),
        cc = percent_encode(challenge),
        state = percent_encode(state),
    )
}

/// The form-urlencoded code-exchange body (`grant_type=authorization_code`).
/// Pure — golden-tested. Note the encoding split: refresh posts JSON.
pub(crate) fn exchange_body(code: &str, redirect_uri: &str, code_verifier: &str) -> String {
    format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        percent_encode(code),
        percent_encode(redirect_uri),
        percent_encode(CODEX_CLIENT_ID),
        percent_encode(code_verifier),
    )
}

/// The form-urlencoded API-key token-exchange body (RFC 8693 grant, subject =
/// the freshly-minted id_token). Pure — golden-tested.
pub(crate) fn api_key_exchange_body(id_token: &str) -> String {
    format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange\
         &client_id={}&requested_token=openai-api-key&subject_token={}\
         &subject_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aid_token",
        percent_encode(CODEX_CLIENT_ID),
        percent_encode(id_token),
    )
}

/// Construct the profile-store auth.json snapshot from minted tokens. Shape
/// mirrors what `codex login` itself writes: `auth_mode` is EXPLICIT because
/// codex's `resolved_mode()` infers ApiKey mode from a bare `OPENAI_API_KEY`
/// when `auth_mode` is absent (HEAD `storage.rs` precedence caveat);
/// `tokens.account_id` is copied from the id_token's `chatgpt_account_id`
/// claim exactly as codex's login does. Pure — shape-tested.
pub(crate) fn build_auth_json(
    tokens: &CodexTokenExchange,
    api_key: Option<&str>,
    now_rfc3339: &str,
) -> Result<Vec<u8>> {
    let account_id = crate::codex::auth::jwt_claims(&tokens.id_token)
        .and_then(|claims| {
            claims
                .get("https://api.openai.com/auth")?
                .get("chatgpt_account_id")?
                .as_str()
                .map(str::to_string)
        })
        .context("the minted id_token carries no chatgpt_account_id claim")?;
    let mut file = serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": tokens.id_token,
            "access_token": tokens.access_token,
            "refresh_token": tokens.refresh_token,
            "account_id": account_id,
        },
        "last_refresh": now_rfc3339,
    });
    if let Some(key) = api_key {
        file["OPENAI_API_KEY"] = serde_json::json!(key);
    }
    let mut out = serde_json::to_vec_pretty(&file)?;
    out.push(b'\n');
    Ok(out)
}

/// Exchange the authorization code for the token triple (form-urlencoded).
fn exchange_code(
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<CodexTokenExchange> {
    let body = exchange_body(code, redirect_uri, code_verifier);
    let mut response = crate::oauth::AGENT
        .post(CODEX_TOKEN_ENDPOINT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(&body)
        .map_err(anyhow::Error::from)?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(anyhow::Error::from)?;
    if status >= 400 {
        return Err(crate::oauth::http_error(status, &text));
    }
    serde_json::from_str(&text).map_err(|e| crate::oauth::token_parse_error(e, status, text.len()))
}

/// Best-effort API-key mint (codex's own `.ok()` semantics — `None` on any
/// failure; the login stands without it).
fn mint_api_key(id_token: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct ExchangeResp {
        access_token: String,
    }
    let body = api_key_exchange_body(id_token);
    let mut response = crate::oauth::AGENT
        .post(CODEX_TOKEN_ENDPOINT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(&body)
        .ok()?;
    if response.status().as_u16() >= 400 {
        return None;
    }
    let text = response.body_mut().read_to_string().ok()?;
    serde_json::from_str::<ExchangeResp>(&text)
        .ok()
        .map(|r| r.access_token)
}

/// Progress milestones, mirroring `oauth_login::LoginProgress`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CodexLoginProgress<'a> {
    AuthorizeUrl(&'a str),
    ExchangingCode,
}

/// Run the browser round-trip and return the minted store-ready snapshot
/// bytes. Pure of profile-store concerns — the caller (actions) validates
/// the name, dedups the account, and writes the store.
pub(crate) fn browser_login_snapshot(progress: impl Fn(CodexLoginProgress<'_>)) -> Result<Vec<u8>> {
    let (verifier, challenge) = new_pkce()?;
    let state = random_b64url(32)?;

    let (listener, port) = bind_loopback(BindPort::Fixed(&CODEX_CALLBACK_PORTS))?;
    let redirect_uri = format!("http://localhost:{port}{CODEX_CALLBACK_PATH}");
    let url = authorize_url(&redirect_uri, &challenge, &state);

    progress(CodexLoginProgress::AuthorizeUrl(&url));
    let _ = crate::platform::open_url(&url);

    let deadline = Instant::now() + Duration::from_secs(LOGIN_TIMEOUT_SECS);
    let code = wait_for_code(&listener, &state, deadline, CODEX_CALLBACK_PATH)?;
    progress(CodexLoginProgress::ExchangingCode);
    let tokens = exchange_code(&code, &redirect_uri, &verifier)?;
    let api_key = mint_api_key(&tokens.id_token);
    let now_iso = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
    build_auth_json(&tokens, api_key.as_deref(), &now_iso)
}

#[cfg(test)]
#[path = "../../tests/inline/codex_login.rs"]
mod tests;
