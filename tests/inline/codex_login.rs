//! CDX-3 R5 wire-shape goldens for the codex browser login. Network legs are
//! manual acceptance (real browser + real exchange are AX's); everything pure
//! is pinned here. Fixtures only — never real tokens.

use super::*;

use crate::testutil::fake_jwt;

#[test]
fn authorize_url_carries_the_codex_param_set() {
    let url = authorize_url("http://localhost:1455/auth/callback", "CHAL", "STATE");
    assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
    assert!(url.contains("response_type=code"));
    assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
    assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    // Scope separators form-encode as `+`; `.` is unreserved and stays.
    assert!(
        url.contains(
            "scope=openid+profile+email+offline_access+api.connectors.read+api.connectors.invoke"
        ),
        "scope drifted: {url}"
    );
    assert!(url.contains("code_challenge=CHAL"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("id_token_add_organizations=true"));
    assert!(url.contains("codex_cli_simplified_flow=true"));
    assert!(url.contains("originator=codex_cli_rs"));
    assert!(url.contains("state=STATE"));
}

#[test]
fn exchange_body_is_form_encoded_authorization_code() {
    let body = exchange_body(
        "the-code",
        "http://localhost:1455/auth/callback",
        "the-verifier",
    );
    assert_eq!(
        body,
        "grant_type=authorization_code&code=the-code\
         &redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\
         &client_id=app_EMoamEEZ73f0CkXaXp7hrann&code_verifier=the-verifier"
    );
}

#[test]
fn api_key_exchange_body_is_the_rfc8693_grant() {
    let body = api_key_exchange_body("jwt-abc");
    assert!(body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange"));
    assert!(body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
    assert!(body.contains("requested_token=openai-api-key"));
    assert!(body.contains("subject_token=jwt-abc"));
    assert!(
        body.contains("subject_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aid_token")
    );
}

fn minted() -> CodexTokenExchange {
    CodexTokenExchange {
        id_token: fake_jwt(&serde_json::json!({
            "email": "new@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "chatgpt_account_id": "acct-new",
            },
        })),
        access_token: fake_jwt(&serde_json::json!({ "exp": 1_900_000_000 })),
        refresh_token: "rt-new".to_string(),
    }
}

#[test]
fn build_auth_json_writes_the_codex_shape_with_explicit_auth_mode() {
    let bytes =
        build_auth_json(&minted(), Some("sk-minted"), "2026-07-16T12:00:00+00:00").expect("build");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    // auth_mode MUST be explicit: a bare OPENAI_API_KEY without it makes
    // codex's resolved_mode() infer ApiKey mode instead of ChatGPT.
    assert_eq!(v["auth_mode"], "chatgpt");
    assert_eq!(v["OPENAI_API_KEY"], "sk-minted");
    assert_eq!(v["tokens"]["refresh_token"], "rt-new");
    assert_eq!(v["tokens"]["account_id"], "acct-new");
    assert_eq!(v["last_refresh"], "2026-07-16T12:00:00+00:00");

    // The lens reads the constructed file exactly like a captured one.
    let lens = crate::codex::CodexAuthFile::parse(&bytes).expect("lens");
    assert_eq!(lens.account_id().as_deref(), Some("acct-new"));
    assert_eq!(lens.email().as_deref(), Some("new@example.com"));
    assert_eq!(lens.plan().as_deref(), Some("plus"));
    assert!(lens.has_login());
}

#[test]
fn build_auth_json_omits_the_api_key_when_the_mint_failed() {
    let bytes = build_auth_json(&minted(), None, "2026-07-16T12:00:00+00:00").expect("build");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(v.get("OPENAI_API_KEY").is_none(), "no null placeholder");
    assert_eq!(v["auth_mode"], "chatgpt");
}

#[test]
fn build_auth_json_requires_an_account_id_claim() {
    let anchorless = CodexTokenExchange {
        id_token: fake_jwt(&serde_json::json!({ "email": "x@example.com" })),
        access_token: "at".to_string(),
        refresh_token: "rt".to_string(),
    };
    let err = build_auth_json(&anchorless, None, "2026-07-16T12:00:00+00:00")
        .expect_err("anchorless id_token must fail");
    assert!(err.to_string().contains("chatgpt_account_id"));
}
