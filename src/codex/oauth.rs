//! CDX-3 codex-side OAuth: standby token refresh against
//! `https://auth.openai.com/oauth/token`. The codex sibling of the claude
//! refresh in `crate::oauth`, sharing its discipline (token-value-free
//! errors, permanent-vs-transient split) but NOT its wire shape — codex
//! refresh is a JSON body with a different client id, and the response's
//! fields are all optional (each overwrites only when present; `last_refresh`
//! always resets). Verified at openai/codex `9ff47868` / codex-cli 0.144.5
//! (`manager.rs::request_chatgpt_token_refresh` + `persist_tokens`).
//!
//! Ownership rule (PLAN.md §0.9): callers refresh ONLY chains clauth
//! exclusively holds — never the live owner (codex advances that chain
//! itself), never a leased CDX-1b profile. The refresh token is single-use
//! with server-side reuse detection; a second consumer kills the chain.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::auth::CodexAuthFile;

/// codex CLI's OAuth client id (`CLIENT_ID` in codex's login crate).
pub(crate) const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The auth.openai.com token endpoint — refresh AND the interactive login's
/// code exchange both post here (refresh as JSON, exchange as form-encoded).
pub(crate) const CODEX_TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";

/// Access-token expiry margin that makes a parked profile due for a standby
/// refresh. Codex access tokens live ~10 days; 48 h keeps a healthy buffer
/// without hot rotation.
const STANDBY_EXP_MARGIN_MS: u64 = 48 * 60 * 60 * 1000;

/// Keep-alive line: refresh when the chain hasn't advanced in this long,
/// even with a healthy access token — the server-side refresh-token TTL is
/// unknown (PLAN.md §0.8.1), and codex's own no-exp fallback is 8 days, so
/// we stay inside it.
const STANDBY_MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// A codex refresh failure, split exactly like [`crate::oauth::RefreshError`]:
/// `Permanent` means the endpoint confirmed the chain is dead (quarantine —
/// `clauth login <name> --codex` is the fix); `Transient` means retry next
/// cadence and never quarantine.
pub(crate) enum CodexRefreshError {
    Permanent(String),
    Transient(anyhow::Error),
}

impl From<CodexRefreshError> for anyhow::Error {
    fn from(e: CodexRefreshError) -> Self {
        match e {
            CodexRefreshError::Permanent(msg) => anyhow::anyhow!(msg),
            CodexRefreshError::Transient(e) => e,
        }
    }
}

/// The refresh response — all fields optional (codex's `RefreshResponse`).
#[derive(Deserialize)]
pub(crate) struct CodexRefreshResponse {
    #[serde(default)]
    pub(crate) id_token: Option<String>,
    #[serde(default)]
    pub(crate) access_token: Option<String>,
    #[serde(default)]
    pub(crate) refresh_token: Option<String>,
}

/// The refresh request body (JSON — unlike the form-encoded login exchange).
/// Pure so the exact wire shape is golden-tested.
pub(crate) fn refresh_body(refresh_token: &str) -> serde_json::Result<String> {
    serde_json::to_string(&serde_json::json!({
        "client_id": CODEX_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    }))
}

/// Whether a refresh rejection confirms the chain is dead. Codex's own
/// classifier (`classify_refresh_token_failure`): permanent ⇔ HTTP 401, OR a
/// body error code (JSON `error.code` or top-level `code`, case-insensitive)
/// naming one of the three dead-chain verdicts. An unconfirmed 4xx stays
/// transient — our own request shape drifting must not quarantine every
/// parked profile (the same reasoning as the claude-side truth table).
pub(crate) fn refresh_failure_is_permanent(status: u16, body: &str) -> bool {
    if status == 401 {
        return true;
    }
    let Some(code) = extract_error_code(body) else {
        return false;
    };
    matches!(
        code.as_str(),
        "refresh_token_expired" | "refresh_token_reused" | "refresh_token_invalidated"
    )
}

/// The error code of an OAuth failure body: JSON `error.code`, falling back
/// to a top-level `code`, lowercased. `None` for non-JSON or codeless bodies.
fn extract_error_code(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let code = v
        .get("error")
        .and_then(|e| e.get("code"))
        .or_else(|| v.get("code"))?
        .as_str()?;
    Some(code.to_lowercase())
}

/// One refresh round trip. The caller MUST hold the per-profile
/// `RotationGuard` across this call (single-use token; see module docs) and
/// must not hold the config mutex or state flock (network round trip).
pub(crate) fn refresh(
    refresh_token: &str,
) -> std::result::Result<CodexRefreshResponse, CodexRefreshError> {
    let body = refresh_body(refresh_token).map_err(|e| CodexRefreshError::Transient(e.into()))?;
    let mut response = crate::oauth::AGENT
        .post(CODEX_TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .send(&body)
        .map_err(|e| CodexRefreshError::Transient(anyhow::Error::from(e)))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| CodexRefreshError::Transient(anyhow::Error::from(e)))?;
    if refresh_failure_is_permanent(status, &text) {
        return Err(CodexRefreshError::Permanent(
            crate::oauth::http_error(status, &text).to_string(),
        ));
    }
    if status >= 400 {
        return Err(CodexRefreshError::Transient(crate::oauth::http_error(
            status, &text,
        )));
    }
    serde_json::from_str(&text).map_err(|e| {
        CodexRefreshError::Transient(crate::oauth::token_parse_error(e, status, text.len()))
    })
}

/// Apply a refresh response to stored auth.json bytes — the surgical sibling
/// of codex's own `persist_tokens`: each of `tokens.{id_token, access_token,
/// refresh_token}` overwrites ONLY when the response carried it;
/// `last_refresh` always resets. The mutation happens on the parsed
/// `serde_json::Value` (preserve_order is on), so every unmodeled field and
/// the key order survive; only whitespace may differ from codex's writer —
/// cosmetic, codex re-parses leniently. Never goes through a typed struct
/// (§0.3 raw round-trip).
pub(crate) fn apply_refresh(
    stored: &[u8],
    resp: &CodexRefreshResponse,
    now_rfc3339: &str,
) -> Result<Vec<u8>> {
    let mut v: serde_json::Value =
        serde_json::from_slice(stored).context("stored codex-auth.json is not JSON")?;
    let obj = v
        .as_object_mut()
        .context("stored codex-auth.json is not a JSON object")?;
    let tokens = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
    let tokens = tokens
        .as_object_mut()
        .context("stored codex-auth.json `tokens` is not an object")?;
    for (key, value) in [
        ("id_token", &resp.id_token),
        ("access_token", &resp.access_token),
        ("refresh_token", &resp.refresh_token),
    ] {
        if let Some(value) = value {
            tokens.insert(key.to_string(), serde_json::json!(value));
        }
    }
    obj.insert("last_refresh".to_string(), serde_json::json!(now_rfc3339));
    let mut out = serde_json::to_vec_pretty(&v).context("failed to serialize refreshed auth")?;
    out.push(b'\n');
    Ok(out)
}

/// Whether a parked profile's stored chain is due for a standby refresh:
/// the access token expires within [`STANDBY_EXP_MARGIN_MS`], OR the chain
/// hasn't advanced (no/old `last_refresh`) in [`STANDBY_MAX_AGE_MS`]. A file
/// with no refresh token can't be refreshed at all — never due.
pub(crate) fn standby_due(auth: &CodexAuthFile, now_ms: u64) -> bool {
    if auth.refresh_token().is_none() {
        return false;
    }
    if let Some(exp_ms) = auth.access_token_exp_ms()
        && u64::try_from(exp_ms).unwrap_or(0) <= now_ms.saturating_add(STANDBY_EXP_MARGIN_MS)
    {
        return true;
    }
    match auth.last_refresh_ms() {
        Some(lr) => lr.saturating_add(STANDBY_MAX_AGE_MS) <= now_ms,
        None => true,
    }
}

#[cfg(test)]
#[path = "../../tests/inline/codex_oauth.rs"]
mod tests;
