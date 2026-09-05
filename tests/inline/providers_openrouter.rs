//! Inline tests for the OpenRouter provider — base-URL matching and the
//! two-endpoint response → display-rows mapping.

use super::*;

use crate::providers::Provider;

// ── Provider::from_base_url dispatch ───────────────────────────────────────────
//
// Asserted through the dispatch, not the module fn: the module fn passing while
// the `from_base_url` arm is missing would silently route an OpenRouter profile
// through the generic scanner, and a mutation that drops the arm must red here.

#[test]
fn from_base_url_dispatches_openrouter() {
    for url in [
        "https://openrouter.ai",
        "https://openrouter.ai/api",
        "https://openrouter.ai/api/v1",
        "https://openrouter.ai/api/v1/chat/completions",
        // Hosts are case-insensitive (RFC 3986).
        "https://OPENROUTER.AI/api",
        // An explicit port is still the provider.
        "https://openrouter.ai:443/api",
    ] {
        assert_eq!(
            Provider::from_base_url(url),
            Some(Provider::OpenRouter),
            "{url}"
        );
    }
}

#[test]
fn from_base_url_rejects_host_extension_and_userinfo() {
    // A bare prefix match would claim these and send the profile's API key to
    // the real provider endpoint.
    assert_eq!(
        Provider::from_base_url("https://openrouter.ai.evil.tld"),
        None
    );
    // Everything before an `@` is userinfo, so this host is `evil.tld`.
    assert_eq!(
        Provider::from_base_url("https://openrouter.ai:443@evil.tld"),
        None
    );
    assert_eq!(Provider::from_base_url("http://openrouter.ai"), None);
    assert_eq!(Provider::from_base_url("https://api.anthropic.com"), None);
}

// ── wire parsing ───────────────────────────────────────────────────────────────
//
// Both fixtures are real captured bodies (2026-08-17, two live regular keys),
// redacted of the label and the user id. The credits body is the key whose
// inference calls answer `402 ... can only afford 0`: its `limit` fields are
// null while the wallet is overdrawn, which is what the null-cap row semantics
// below pin.

const KEY_BODY: &str = r#"{
    "data": {
        "label": "sk-or-v1-e3d...94f",
        "is_management_key": false,
        "is_provisioning_key": false,
        "limit": null,
        "limit_reset": null,
        "limit_remaining": null,
        "include_byok_in_limit": false,
        "usage": 162.870592992,
        "usage_daily": 0.000001536,
        "usage_weekly": 0.000001536,
        "usage_monthly": 0.003722806,
        "byok_usage": 9.49797775,
        "byok_usage_daily": 0,
        "byok_usage_weekly": 0,
        "byok_usage_monthly": 0,
        "is_free_tier": false,
        "expires_at": null,
        "rate_limit": {"requests": -1, "interval": "10s"}
    }
}"#;

const CREDITS_BODY: &str = r#"{
    "data": {"total_credits": 600.815, "total_usage": 601.014979078}
}"#;

#[test]
fn both_responses_parse_wire_shape() {
    let credits: CreditsEnvelope = serde_json::from_str(CREDITS_BODY).expect("parse credits");
    assert_eq!(credits.data.total_credits, 600.815);
    assert_eq!(credits.data.total_usage, 601.014979078);

    let key: KeyEnvelope = serde_json::from_str(KEY_BODY).expect("parse key response");
    assert_eq!(key.data.limit, None);
    assert_eq!(key.data.limit_remaining, None);
    assert!(!key.data.is_free_tier);
    // Every period field the mapping renders, so a serde rename or typo reds
    // here rather than silently zeroing a row.
    assert_eq!(key.data.usage_daily, 0.000001536);
    assert_eq!(key.data.usage_weekly, 0.000001536);
    assert_eq!(key.data.usage_monthly, 0.003722806);
}

#[test]
fn credits_without_required_numbers_fails_to_parse() {
    // Both numbers are required: a degraded wallet body must never invent one.
    assert!(serde_json::from_str::<CreditsEnvelope>(r#"{"data":{}}"#).is_err());
}

#[test]
fn key_response_without_data_fails_to_parse() {
    // `data` is required: an error envelope carrying no key info must never
    // count as usable usage.
    assert!(serde_json::from_str::<KeyEnvelope>("{}").is_err());
}

// ── response → rows ────────────────────────────────────────────────────────────

#[test]
fn stats_builds_wallet_rows() {
    let credits: CreditsEnvelope = serde_json::from_str(CREDITS_BODY).unwrap();
    let key: KeyEnvelope = serde_json::from_str(KEY_BODY).unwrap();
    let stats = stats(&credits.data, Some(&key.data));
    // The live account is overdrawn: the rows render, but the reachability
    // dot must read red (every paid call 402s) and the refusal rides beside
    // the figures rather than replacing them.
    assert!(!stats.is_available);
    let last = stats.rows.last().expect("refusal row");
    assert_eq!(last.kind, StatRowKind::Danger);
    assert_eq!(last.value, crate::providers::LOW_BALANCE);
    // Heading + 3 wallet rows + 3 period rows, plus that refusal. No cap, no
    // free tier.
    assert_eq!(stats.rows.len(), 8);
    assert_eq!(stats.rows[0].kind, StatRowKind::Heading);
    assert_eq!(stats.rows[0].label, "credits");
    // The literal, not the constant: this row's label is a cross-module contract
    // (the MCP roster's wallet rank matches on it), so a rename has to red here
    // rather than follow silently.
    assert_eq!(stats.rows[1].label, "api balance");
    // The live overdrawn account: usage exceeds the purchased credits.
    assert_eq!(stats.rows[1].value, "-0.20 USD");
    assert_eq!(stats.rows[1].kind, StatRowKind::Danger);
    // `2..7`, not `2..`: the trailing entry is the refusal asserted above, and
    // it carries no label.
    let labels: Vec<&str> = stats.rows[2..7].iter().map(|r| r.label.as_str()).collect();
    assert_eq!(
        labels,
        ["used", "purchased", "today", "this week", "this month"]
    );
    assert_eq!(stats.rows[2].value, "601.01 USD");
    assert_eq!(stats.rows[3].value, "600.82 USD");
    assert_eq!(stats.rows[4].value, "0.00 USD");
    assert_eq!(stats.rows[5].value, "0.00 USD");
    assert_eq!(stats.rows[6].value, "0.00 USD");
}

#[test]
fn remaining_danger_boundary_tracks_the_rendered_value() {
    // Danger must agree with what the row SAYS: anything under half a cent
    // (an overdrawn account included) renders as `0.00 USD` or worse, so an
    // exact `== 0.0` test would leave a spent key reading as a healthy one.
    // A full cent still formats as `0.01 USD` and stays Body.
    for (total, used, expect_kind, expect_value, expect_available) in [
        (100.0, 100.0, StatRowKind::Danger, "0.00 USD", false),
        (100.0, 99.999, StatRowKind::Danger, "0.00 USD", false),
        (100.0, 100.2, StatRowKind::Danger, "-0.20 USD", false),
        (100.0, 99.99, StatRowKind::Body, "0.01 USD", true),
    ] {
        let credits = CreditsData {
            total_credits: total,
            total_usage: used,
        };
        let stats = stats(&credits, None);
        assert_eq!(stats.rows[1].kind, expect_kind, "total {total} used {used}");
        assert_eq!(
            stats.rows[1].value, expect_value,
            "total {total} used {used}"
        );
        assert_eq!(
            stats.is_available, expect_available,
            "total {total} used {used}"
        );
    }
}

#[test]
fn key_cap_and_free_tier_rows_append_when_present() {
    let credits = CreditsData {
        total_credits: 100.0,
        total_usage: 40.0,
    };
    let key = KeyData {
        limit: Some(50.0),
        limit_remaining: Some(10.0),
        usage_daily: 0.0,
        usage_weekly: 0.0,
        usage_monthly: 0.0,
        is_free_tier: true,
    };
    let stats = stats(&credits, Some(&key));
    let rows: Vec<(&str, &str, StatRowKind)> = stats
        .rows
        .iter()
        .map(|r| (r.label.as_str(), r.value.as_str(), r.kind))
        .collect();
    assert!(rows.contains(&("key limit", "50.00 USD", StatRowKind::Body)));
    assert!(rows.contains(&("key limit left", "10.00 USD", StatRowKind::Body)));
    assert!(rows.contains(&("free tier", "", StatRowKind::Faint)));
}

#[test]
fn key_endpoint_absent_drops_only_the_key_rows() {
    let credits = CreditsData {
        total_credits: 100.0,
        total_usage: 40.0,
    };
    let stats = stats(&credits, None);
    let labels: Vec<&str> = stats.rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["credits", "api balance", "used", "purchased"]);
    assert_eq!(stats.rows[1].value, "60.00 USD");
}
