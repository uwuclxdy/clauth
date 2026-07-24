//! CDX-3 wire-shape tests: refresh body golden, failure-taxonomy truth table,
//! and the surgical `apply_refresh` Value mutation (raw round-trip §0.3 —
//! unmodeled fields and key order survive; only the token fields move).
//! Fixtures only — never real tokens, never the real `~/.codex`.

use super::*;

use crate::testutil::fake_jwt;

// --- refresh body ----------------------------------------------------------

#[test]
fn refresh_body_matches_codex_wire_shape() {
    // Verbatim RefreshRequest shape at openai/codex 9ff47868 (JSON, not form).
    let body = refresh_body("rt-alpha").expect("serialize");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        v,
        serde_json::json!({
            "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
            "grant_type": "refresh_token",
            "refresh_token": "rt-alpha",
        })
    );
}

// --- failure taxonomy ------------------------------------------------------

#[test]
fn refresh_failure_truth_table() {
    // Permanent ⇔ 401, OR a confirmed dead-chain code in the body (JSON
    // `error.code` or top-level `code`, case-insensitive) — codex's own
    // classify_refresh_token_failure. Everything else is transient.
    let dead = |code: &str| format!(r#"{{"error":{{"code":"{code}"}}}}"#);
    let cases: &[(u16, &str, bool)] = &[
        (401, "", true),
        (401, "anything at all", true),
        (400, &dead("refresh_token_expired"), true),
        (400, &dead("refresh_token_reused"), true),
        (400, &dead("refresh_token_invalidated"), true),
        (403, &dead("refresh_token_reused"), true),
        // Top-level `code` variant + case-insensitivity.
        (400, r#"{"code":"REFRESH_TOKEN_EXPIRED"}"#, true),
        // Unconfirmed 4xx: our own request shape drifting must not
        // quarantine every parked profile (same reasoning as the claude
        // refresh_rejection_is_terminal table).
        (400, r#"{"error":{"code":"invalid_request"}}"#, false),
        (400, "not json", false),
        (403, "", false),
        (429, &dead("refresh_token_expired"), true), // code trumps status
        (429, "", false),
        (500, "", false),
        (503, r#"{"error":{"code":"server_error"}}"#, false),
    ];
    for (status, body, permanent) in cases {
        assert_eq!(
            refresh_failure_is_permanent(*status, body),
            *permanent,
            "status={status} body={body}"
        );
    }
}

// --- apply_refresh ---------------------------------------------------------

fn stored_fixture() -> Vec<u8> {
    let id_token = fake_jwt(&serde_json::json!({
        "email": "alpha@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_account_id": "acct-alpha",
        },
    }));
    // Deliberately unmodeled + oddly-ordered fields to pin the round-trip.
    serde_json::to_vec_pretty(&serde_json::json!({
        "zeta_future_field": { "nested": [1, 2, 3] },
        "OPENAI_API_KEY": "sk-fixture",
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": "at-old",
            "refresh_token": "rt-old",
            "account_id": "acct-alpha",
            "future_token_field": true,
        },
        "last_refresh": "2026-07-01T00:00:00Z",
        "agent_identity": { "kind": "cli" },
    }))
    .expect("fixture")
}

#[test]
fn apply_refresh_overwrites_only_present_fields_and_stamps_last_refresh() {
    let resp = CodexRefreshResponse {
        id_token: None,
        access_token: Some("at-new".to_string()),
        refresh_token: Some("rt-new".to_string()),
    };
    let out = apply_refresh(&stored_fixture(), &resp, "2026-07-16T12:00:00Z").expect("apply");
    let v: serde_json::Value = serde_json::from_slice(&out).expect("json");
    // Overwritten:
    assert_eq!(v["tokens"]["access_token"], "at-new");
    assert_eq!(v["tokens"]["refresh_token"], "rt-new");
    assert_eq!(v["last_refresh"], "2026-07-16T12:00:00Z");
    // Absent in the response → untouched (id_token keeps the old JWT):
    assert!(
        v["tokens"]["id_token"]
            .as_str()
            .expect("id_token")
            .contains('.')
    );
    // Unmodeled fields at both levels survive verbatim:
    assert_eq!(
        v["zeta_future_field"]["nested"],
        serde_json::json!([1, 2, 3])
    );
    assert_eq!(v["tokens"]["future_token_field"], true);
    assert_eq!(v["agent_identity"]["kind"], "cli");
    assert_eq!(v["OPENAI_API_KEY"], "sk-fixture");
    assert_eq!(v["tokens"]["account_id"], "acct-alpha");
}

#[test]
fn apply_refresh_preserves_key_order() {
    let resp = CodexRefreshResponse {
        id_token: None,
        access_token: Some("at-new".to_string()),
        refresh_token: None,
    };
    let out = apply_refresh(&stored_fixture(), &resp, "2026-07-16T12:00:00Z").expect("apply");
    let text = String::from_utf8(out).expect("utf8");
    // serde_json preserve_order keeps insertion order: the odd leading field
    // must still lead, and OPENAI_API_KEY must still precede auth_mode.
    let zeta = text.find("zeta_future_field").expect("zeta");
    let key = text.find("OPENAI_API_KEY").expect("api key");
    let mode = text.find("auth_mode").expect("auth_mode");
    assert!(zeta < key && key < mode, "key order drifted:\n{text}");
}

#[test]
fn apply_refresh_rejects_non_object_input() {
    let resp = CodexRefreshResponse {
        id_token: None,
        access_token: Some("at".to_string()),
        refresh_token: None,
    };
    assert!(apply_refresh(b"[]", &resp, "2026-07-16T12:00:00Z").is_err());
    assert!(apply_refresh(b"not json", &resp, "2026-07-16T12:00:00Z").is_err());
}

#[test]
fn apply_refresh_creates_tokens_object_when_missing() {
    // Mirrors codex's own persist (tokens.get_or_insert) — a shell file that
    // somehow reaches apply still round-trips instead of erroring.
    let resp = CodexRefreshResponse {
        id_token: Some("jwt-new".to_string()),
        access_token: Some("at-new".to_string()),
        refresh_token: Some("rt-new".to_string()),
    };
    let out =
        apply_refresh(br#"{"auth_mode":"chatgpt"}"#, &resp, "2026-07-16T12:00:00Z").expect("apply");
    let v: serde_json::Value = serde_json::from_slice(&out).expect("json");
    assert_eq!(v["tokens"]["access_token"], "at-new");
    assert_eq!(v["auth_mode"], "chatgpt");
}

// --- standby due predicate ---------------------------------------------------

fn auth_with(exp_secs: i64, last_refresh: Option<&str>) -> CodexAuthFile {
    let access_token = fake_jwt(&serde_json::json!({ "exp": exp_secs }));
    let mut file = serde_json::json!({
        "tokens": {
            "access_token": access_token,
            "refresh_token": "rt-x",
            "account_id": "acct-x",
        },
    });
    if let Some(lr) = last_refresh {
        file["last_refresh"] = serde_json::json!(lr);
    }
    CodexAuthFile::parse(file.to_string().as_bytes()).expect("parse")
}

#[test]
fn standby_due_when_access_token_expires_within_margin() {
    let now_ms: u64 = 1_752_600_000_000; // arbitrary anchor
    let now_secs = (now_ms / 1000) as i64;
    let recent = crate::usage::epoch_secs_to_iso(now_secs - 86_400);
    // Exp 10 days out, refreshed a day ago → parked and healthy → not due.
    let fresh = auth_with(now_secs + 10 * 86_400, Some(&recent));
    assert!(!standby_due(&fresh, now_ms));
    // Exp inside the 48h margin → due regardless of last_refresh.
    let expiring = auth_with(now_secs + 3_600, Some(&recent));
    assert!(standby_due(&expiring, now_ms));
    // Already expired → due.
    let expired = auth_with(now_secs - 3_600, Some(&recent));
    assert!(standby_due(&expired, now_ms));
}

#[test]
fn standby_due_when_last_refresh_stale_or_missing() {
    let now_ms: u64 = 1_752_600_000_000;
    let now_secs = (now_ms / 1000) as i64;
    let old = crate::usage::epoch_secs_to_iso(now_secs - 8 * 86_400);
    // Healthy exp but the chain hasn't been advanced in > 7d → keep-alive due.
    let stale = auth_with(now_secs + 10 * 86_400, Some(&old));
    assert!(standby_due(&stale, now_ms));
    // No last_refresh at all → due (unknown age, refresh to learn it).
    let unknown = auth_with(now_secs + 10 * 86_400, None);
    assert!(standby_due(&unknown, now_ms));
}

#[test]
fn standby_not_due_without_a_refresh_token() {
    let now_ms: u64 = 1_752_600_000_000;
    let now_secs = (now_ms / 1000) as i64;
    let access_token = fake_jwt(&serde_json::json!({ "exp": now_secs - 10 }));
    let auth = CodexAuthFile::parse(
        serde_json::json!({ "tokens": { "access_token": access_token } })
            .to_string()
            .as_bytes(),
    )
    .expect("parse");
    assert!(!standby_due(&auth, now_ms));
}
