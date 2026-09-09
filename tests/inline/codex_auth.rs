//! Lens tests over fixture auth.json files. Fake, unsigned JWTs are built
//! locally — never real tokens, never the real `~/.codex`.

use super::*;

use crate::testutil::{b64url_nopad as enc, fake_jwt as shared_fake_jwt};

fn fake_jwt(claims: &serde_json::Value) -> String {
    shared_fake_jwt(claims)
}

fn full_fixture() -> Vec<u8> {
    let id_token = fake_jwt(&serde_json::json!({
        "email": "alpha@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_account_id": "acct-alpha",
        },
    }));
    let access_token = fake_jwt(&serde_json::json!({ "exp": 1_800_000_000 }));
    serde_json::json!({
        "OPENAI_API_KEY": "sk-fixture",
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": "rt-alpha",
            "account_id": "acct-alpha",
        },
        "last_refresh": "2026-07-16T00:00:00Z",
        "agent_identity": { "future": "field" },
    })
    .to_string()
    .into_bytes()
}

#[test]
fn lens_reads_identity_from_a_full_auth_json() {
    let auth = CodexAuthFile::parse(&full_fixture()).expect("parse fixture");
    assert_eq!(auth.account_id().as_deref(), Some("acct-alpha"));
    assert_eq!(auth.email().as_deref(), Some("alpha@example.com"));
    assert_eq!(auth.plan().as_deref(), Some("pro"));
    assert_eq!(auth.access_token_exp_ms(), Some(1_800_000_000_000));
    assert_eq!(auth.refresh_token(), Some("rt-alpha"));
    assert!(auth.has_login());
    assert!(auth.fingerprint().is_some());
}

// Files minted before the top-level `tokens.account_id` copy existed carry
// the anchor only inside the id_token — the lens must fall back to it.
#[test]
fn account_id_falls_back_to_the_id_token_claim() {
    let id_token = fake_jwt(&serde_json::json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": "acct-old" },
    }));
    let bytes = serde_json::json!({
        "tokens": { "id_token": id_token, "access_token": "at-x" },
    })
    .to_string()
    .into_bytes();
    let auth = CodexAuthFile::parse(&bytes).expect("parse");
    assert_eq!(auth.account_id().as_deref(), Some("acct-old"));
}

#[test]
fn email_falls_back_to_the_profile_claim() {
    let id_token = fake_jwt(&serde_json::json!({
        "https://api.openai.com/profile": { "email": "claim@example.com" },
    }));
    let bytes = serde_json::json!({ "tokens": { "id_token": id_token } })
        .to_string()
        .into_bytes();
    let auth = CodexAuthFile::parse(&bytes).expect("parse");
    assert_eq!(auth.email().as_deref(), Some("claim@example.com"));
}

// Blank/absent tokens = a logged-out shell: nothing to protect, no identity.
#[test]
fn blank_tokens_mean_no_login_and_no_fingerprint() {
    let bytes = serde_json::json!({
        "tokens": { "access_token": "", "refresh_token": "" },
    })
    .to_string()
    .into_bytes();
    let auth = CodexAuthFile::parse(&bytes).expect("parse");
    assert!(!auth.has_login());
    assert!(auth.fingerprint().is_none());
    assert!(auth.account_id().is_none());
}

#[test]
fn parse_rejects_non_object_content() {
    assert!(CodexAuthFile::parse(b"[1,2,3]").is_err());
    assert!(CodexAuthFile::parse(b"not json").is_err());
}

// Lenient decode: malformed JWTs answer None, never panic; padded base64url
// (older encoders pad) still decodes.
#[test]
fn jwt_claims_is_lenient() {
    assert!(jwt_claims("not-a-jwt").is_none());
    assert!(jwt_claims("a.!!!!.c").is_none());
    assert!(jwt_claims("").is_none());

    let claims = serde_json::json!({ "email": "pad@example.com" });
    let mut payload = enc(claims.to_string().as_bytes());
    while !payload.len().is_multiple_of(4) {
        payload.push('=');
    }
    let padded = format!("{}.{payload}.sig", enc(b"{}"));
    assert_eq!(
        jwt_claims(&padded)
            .and_then(|c| c.get("email").and_then(|v| v.as_str().map(str::to_string))),
        Some("pad@example.com".to_string())
    );
}

// The two fingerprints must key on the access token: same token → same hash,
// different token → different hash (the follow/switch memos rely on this).
#[test]
fn fingerprint_tracks_the_access_token() {
    let a1 = CodexAuthFile::parse(br#"{"tokens":{"access_token":"at-1"}}"#).unwrap();
    let a2 =
        CodexAuthFile::parse(br#"{"tokens":{"access_token":"at-1","account_id":"x"}}"#).unwrap();
    let b = CodexAuthFile::parse(br#"{"tokens":{"access_token":"at-2"}}"#).unwrap();
    assert_eq!(a1.fingerprint(), a2.fingerprint());
    assert_ne!(a1.fingerprint(), b.fingerprint());
}
