//! Inline tests for the DeepSeek provider — wire-shape parsing and the
//! response → display-rows mapping.

use super::*;

#[test]
fn deepseek_response_parses_wire_shape() {
    // Shape per https://api-docs.deepseek.com/api/get-user-balance
    let json = r#"{
        "is_available": true,
        "balance_infos": [{
            "currency": "USD",
            "total_balance": "110.00",
            "granted_balance": "10.00",
            "topped_up_balance": "100.00"
        }]
    }"#;
    let raw: DeepSeekResponse = serde_json::from_str(json).expect("parse balance response");
    assert!(raw.is_available);
    assert_eq!(raw.balance_infos.len(), 1);
    assert_eq!(raw.balance_infos[0].currency, "USD");
}

#[test]
fn stats_builds_heading_and_body_rows() {
    let raw = DeepSeekResponse {
        is_available: true,
        balance_infos: vec![DeepSeekBalance {
            currency: "USD".to_string(),
            total_balance: "110.00".to_string(),
            granted_balance: "10.00".to_string(),
            topped_up_balance: "100.00".to_string(),
        }],
    };
    let stats = stats(&raw);
    assert!(stats.is_available);
    assert_eq!(stats.rows.len(), 4);
    assert_eq!(stats.rows[0].kind, StatRowKind::Heading);
    assert_eq!(stats.rows[0].label, "USD balance");
    // The literal, not the constant: this row's label is a cross-module contract
    // (the overview's balance column and the MCP roster's wallet rank both match
    // on it), so a rename has to red here rather than follow silently.
    assert_eq!(stats.rows[1].label, "api balance");
    assert_eq!(stats.rows[1].value, "110.00 USD");
    assert!(stats.rows[1..].iter().all(|r| r.kind == StatRowKind::Body));
}

#[test]
fn stats_unfunded_with_no_wallets_carries_the_refusal_alone() {
    let raw = DeepSeekResponse {
        is_available: false,
        balance_infos: vec![],
    };
    let stats = stats(&raw);
    assert!(!stats.is_available);
    assert_eq!(stats.rows.len(), 1);
    assert_eq!(stats.rows[0].kind, StatRowKind::Danger);
    assert!(stats.rows[0].label.is_empty());
    assert_eq!(stats.rows[0].value, crate::providers::LOW_BALANCE);
}

/// `is_available: false` is DeepSeek's verdict that the balance cannot fund a
/// call, and the response still carries the wallets. Dropping them left the
/// reader unable to see how short the account was, and the old copy claimed
/// clauth could not read a figure it had in hand.
#[test]
fn stats_unfunded_keeps_the_wallet_rows_beside_the_refusal() {
    let raw = DeepSeekResponse {
        is_available: false,
        balance_infos: vec![DeepSeekBalance {
            currency: "CNY".to_string(),
            total_balance: "0.00".to_string(),
            granted_balance: "0.00".to_string(),
            topped_up_balance: "0.00".to_string(),
        }],
    };
    let stats = stats(&raw);
    assert!(!stats.is_available);
    // The four rows the available path builds, plus the refusal.
    assert_eq!(stats.rows.len(), 5);
    assert_eq!(stats.rows[1].label, "api balance");
    assert_eq!(stats.rows[1].value, "0.00 CNY");
    let last = stats.rows.last().expect("refusal row");
    assert_eq!(last.kind, StatRowKind::Danger);
    assert_eq!(last.value, crate::providers::LOW_BALANCE);
}

#[test]
fn stats_available_but_empty_yields_no_rows() {
    let raw = DeepSeekResponse {
        is_available: true,
        balance_infos: vec![],
    };
    let stats = stats(&raw);
    assert!(stats.is_available);
    assert!(stats.rows.is_empty());
}
