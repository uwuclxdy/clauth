//! CDX-6: `wham/usage` response parsing — all offline (fixture bodies only;
//! the HTTP call itself is a thin wrapper over the shared agent and is never
//! exercised against the real backend from tests).

use super::*;

/// The LIVE backend shape, captured verbatim (scrubbed) 2026-07-22:
/// `limit_window_seconds` (not minutes), `reset_at` (not resets_at), the
/// verdict at the TOP level, `secondary_window: null`, plus the credit and
/// spend blocks the parser must ignore. The weekly 604800s window must route
/// to the 7d slot with its absolute reset.
#[test]
fn parses_the_live_backend_shape() {
    let now = 1_785_000_000i64;
    let body = r#"{
        "user_id": "user-x", "account_id": "user-x", "email": "x@example.com",
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
                "used_percent": 11,
                "limit_window_seconds": 604800,
                "reset_after_seconds": 572214,
                "reset_at": 1785258152
            },
            "secondary_window": null
        },
        "additional_rate_limits": [{"limit_name": "GPT-5.3-Codex-Spark"}],
        "credits": {"has_credits": false, "balance": "0"},
        "spend_control": {"reached": false},
        "rate_limit_reached_type": null,
        "rate_limit_reset_credits": {"available_count": 0}
    }"#;
    let polled = parse_wham_usage(body.as_bytes(), now).expect("parse");
    let info = polled.info;
    let seven_day = info
        .seven_day
        .expect("604800s window routes to the weekly slot");
    assert!((seven_day.utilization - 11.0).abs() < f64::EPSILON);
    let secs = crate::usage::iso_to_epoch_secs(seven_day.resets_at.as_deref().expect("reset"))
        .expect("iso");
    assert_eq!(
        secs, 1_785_258_152,
        "absolute reset_at wins over the relative delta"
    );
    assert!(info.five_hour.is_none(), "no session window in this shape");
    assert!(info.codex_rate_limit_reached.is_none());
    assert_eq!(
        polled.plan_type.as_deref(),
        Some("pro"),
        "the live plan tier rides the same response"
    );
}

/// A top-level verdict (the live shape's spelling) reaches the published
/// snapshot when the block itself carries none.
#[test]
fn top_level_verdict_is_adopted() {
    let now = 1_785_000_000i64;
    let body = r#"{
        "rate_limit": {
            "primary_window": {"used_percent": 100, "limit_window_seconds": 604800, "reset_at": 1785258152}
        },
        "rate_limit_reached_type": "primary"
    }"#;
    let polled = parse_wham_usage(body.as_bytes(), now).expect("parse");
    let info = polled.info;
    // "primary" names the raw window, which routed WEEKLY — route_windows
    // republishes it as the slot-equivalent verdict.
    assert_eq!(info.codex_rate_limit_reached.as_deref(), Some("secondary"));
}

/// The JSONL-side spellings (`rate_limits`/`primary`/`secondary`) parse too —
/// the aliases keep either backend spelling working.
#[test]
fn parses_the_jsonl_side_spellings() {
    let now = 1_700_000_000i64;
    let body = format!(
        r#"{{
            "rate_limits": {{
                "primary": {{"used_percent": 12.0, "resets_at": {r1}, "window_minutes": 300}},
                "secondary": {{"used_percent": 88.0, "resets_at": {r2}, "window_minutes": 10080}},
                "rate_limit_reached_type": "secondary"
            }}
        }}"#,
        r1 = now + 3600,
        r2 = now + 5 * 86_400
    );
    let polled = parse_wham_usage(body.as_bytes(), now).expect("parse");
    let info = polled.info;
    assert!((info.five_hour.expect("short slot").utilization - 12.0).abs() < f64::EPSILON);
    assert!((info.seven_day.expect("weekly slot").utilization - 88.0).abs() < f64::EPSILON);
    assert_eq!(info.codex_rate_limit_reached.as_deref(), Some("secondary"));
}

/// A relative `resets_in_seconds` (no absolute stamp) normalizes to
/// now + delta.
#[test]
fn relative_reset_normalizes_to_absolute() {
    let now = 1_700_000_000i64;
    let body = r#"{
        "rate_limit": {
            "primary_window": {"used_percent": 5.0, "resets_in_seconds": 86400, "window_minutes": 10080}
        }
    }"#;
    let polled = parse_wham_usage(body.as_bytes(), now).expect("parse");
    let info = polled.info;
    let iso = info
        .seven_day
        .expect("weekly")
        .resets_at
        .expect("normalized reset");
    let secs = crate::usage::iso_to_epoch_secs(&iso).expect("parse iso");
    assert_eq!(secs, now + 86_400);
}

/// Shape drift fails LOUD (an error the scheduler paces), never a silent
/// empty snapshot that would overwrite good cached data with zeros.
#[test]
fn unrecognized_shapes_error_instead_of_publishing_zeros() {
    let now = 1_700_000_000i64;
    assert!(matches!(
        parse_wham_usage(b"not json", now),
        Err(PollError::Other(_))
    ));
    assert!(matches!(
        parse_wham_usage(br#"{"entirely": "different"}"#, now),
        Err(PollError::Other(_))
    ));
    assert!(matches!(
        parse_wham_usage(br#"{"rate_limit": {}}"#, now),
        Err(PollError::Other(_))
    ));
}
