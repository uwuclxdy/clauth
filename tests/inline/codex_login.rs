#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The codex PKCE login's pure wire facts, held against the values read from
//! openai/codex at tag rust-v0.145.0. The network legs (exchange, api-key
//! mint) are covered where the daemon standby tests exercise the shared agent;
//! here the assembly and the URL/claim shapes are pinned.

use super::*;

#[test]
fn the_authorize_url_carries_the_verified_params() {
    let url = authorize_url("http://localhost:1455/auth/callback", "CHAL", "STATE");
    assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
    for needle in [
        "response_type=code",
        &format!("client_id={CODEX_CLIENT_ID}"),
        "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
        "code_challenge=CHAL",
        "code_challenge_method=S256",
        "id_token_add_organizations=true",
        "codex_cli_simplified_flow=true",
        "state=STATE",
        "originator=codex_cli_rs",
    ] {
        assert!(url.contains(needle), "missing {needle} in {url}");
    }
    // The verified scope set, percent-encoded.
    assert!(
        url.contains("scope=openid%20profile%20email%20offline_access"),
        "scope: {url}"
    );
}

/// The account id is the `chatgpt_account_id` claim nested under the
/// id_token's `https://api.openai.com/auth` object — codex's exact nesting.
#[test]
fn the_account_id_comes_from_the_nested_claim() {
    let payload = crate::oauth_login::base64url_nopad(
        br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-123"}}"#,
    );
    let jwt = format!("h.{payload}.s");
    assert_eq!(chatgpt_account_id(&jwt).as_deref(), Some("acc-123"));

    // A flat claim (not nested) is NOT read — the nesting is the contract.
    let flat = crate::oauth_login::base64url_nopad(br#"{"chatgpt_account_id":"nope"}"#);
    assert_eq!(chatgpt_account_id(&format!("h.{flat}.s")), None);
}

/// The assembled auth.json: explicit `auth_mode` (codex infers ApiKey from a
/// bare key otherwise), the chain, the account id folded in, and no
/// OPENAI_API_KEY when the secondary exchange yielded none.
#[test]
fn the_auth_json_is_codexs_shape() {
    let payload = crate::oauth_login::base64url_nopad(
        br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-9"}}"#,
    );
    let tok = CodeExchange {
        id_token: format!("h.{payload}.s"),
        access_token: "at.x".into(),
        refresh_token: "rt.x".into(),
    };
    let outcome = assemble_auth_json(tok, None);
    let v: serde_json::Value = serde_json::from_slice(&outcome.auth_json).unwrap();
    assert_eq!(v["auth_mode"], "chatgpt", "explicit, never inferred");
    assert_eq!(v["tokens"]["access_token"], "at.x");
    assert_eq!(v["tokens"]["refresh_token"], "rt.x");
    assert_eq!(v["tokens"]["account_id"], "acc-9");
    assert!(v["last_refresh"].is_string());
    assert!(
        v.get("OPENAI_API_KEY").is_none(),
        "no api key when the exchange is skipped/offline"
    );
    assert_eq!(outcome.account_id.as_deref(), Some("acc-9"));
    // The chain reparses through the same model the runtime uses.
    let parsed = crate::codex_auth::CodexAuth::parse(&outcome.auth_json).expect("parse");
    assert_eq!(parsed.refresh_token(), Some("rt.x"));
    assert_eq!(parsed.account_id(), Some("acc-9"));
}

/// The encoding trap the spec pins: the authorization-code exchange is
/// form-urlencoded, while the refresh at the SAME endpoint is JSON. A
/// mutation swapping the exchange to JSON reds this.
#[test]
fn the_code_exchange_is_form_urlencoded_unlike_the_json_refresh() {
    let body = code_exchange_body(
        "the-code",
        "the-verifier",
        "http://localhost:1455/auth/callback",
    );
    assert!(body.starts_with("grant_type=authorization_code&"), "{body}");
    assert!(body.contains("&code=the-code&"), "{body}");
    assert!(body.contains("&code_verifier=the-verifier"), "{body}");
    assert!(
        !body.trim_start().starts_with('{'),
        "form, never JSON: {body}"
    );

    // The refresh body IS json, at the same endpoint — the two must not drift.
    let refresh = crate::codex_auth::CODEX_TOKEN_URL;
    assert_eq!(refresh, "https://auth.openai.com/oauth/token");
}
