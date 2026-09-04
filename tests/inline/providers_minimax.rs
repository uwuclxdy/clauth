//! Inline tests for the MiniMax provider — `token_plan/remains` → bars/rows,
//! the remaining→utilization inversion, and the bucket-selection rule. Parsed
//! against the real captured wire shape.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::providers::Provider;

// Real `GET /v1/token_plan/remains` shape: two plan buckets, `general` (what
// Claude Code spends) at 38% interval / 69% weekly remaining, and the unrelated
// `video` product untouched at 100%.
const REMAINS: &str = r#"{
  "model_remains":[
    {"start_time":1788516000000,"end_time":1788534000000,"remains_time":6651988,
     "current_interval_total_count":0,"current_interval_usage_count":0,
     "model_name":"general",
     "current_weekly_total_count":0,"current_weekly_usage_count":0,
     "weekly_start_time":1788134400000,"weekly_end_time":1788739200000,
     "weekly_remains_time":211851988,
     "current_interval_status":1,"current_interval_remaining_percent":38,
     "current_weekly_status":1,"current_weekly_remaining_percent":69},
    {"start_time":1788480000000,"end_time":1788566400000,"remains_time":39051988,
     "current_interval_total_count":0,"current_interval_usage_count":0,
     "model_name":"video",
     "current_weekly_total_count":0,"current_weekly_usage_count":0,
     "weekly_start_time":1788134400000,"weekly_end_time":1788739200000,
     "weekly_remains_time":211851988,
     "current_interval_status":3,"current_interval_remaining_percent":100,
     "current_weekly_status":3,"current_weekly_remaining_percent":100}
  ],
  "base_resp":{"status_code":0,"status_msg":"success"}
}"#;

fn parsed(json: &str) -> Vec<ModelRemains> {
    serde_json::from_str::<RemainsResponse>(json)
        .unwrap()
        .model_remains
}

// ── URL matching ──────────────────────────────────────────────────────────────

#[test]
fn minimax_matches_its_anthropic_base_url() {
    assert_eq!(
        Provider::from_base_url("https://api.minimax.io/anthropic"),
        Some(Provider::MiniMax)
    );
    assert_eq!(
        Provider::from_base_url("https://api.minimax.io"),
        Some(Provider::MiniMax)
    );
}

#[test]
fn minimax_rejects_host_extensions_and_the_cn_host() {
    // A host extension would send this account's api key to the real MiniMax.
    assert_eq!(
        Provider::from_base_url("https://api.minimax.io.evil.tld/anthropic"),
        None
    );
    // The China-region endpoint is a different account on a host the typed
    // fetch cannot address (it resolves ORIGIN from a constant), so it stays
    // with the generic scanner rather than being claimed here.
    assert_eq!(Provider::from_base_url("https://api.minimaxi.com"), None);
}

// ── Wire → stats ──────────────────────────────────────────────────────────────

#[test]
fn bars_invert_remaining_into_utilization_off_the_general_bucket() {
    let bars = bars(&parsed(REMAINS));
    assert_eq!(bars.len(), 2, "one 5h bar and one 7d bar");
    assert_eq!(bars[0].label, "5h");
    assert!((bars[0].pct - 62.0).abs() < f64::EPSILON, "100 - 38");
    assert_eq!(bars[1].label, "7d");
    assert!((bars[1].pct - 31.0).abs() < f64::EPSILON, "100 - 69");
}

#[test]
fn bars_carry_the_window_end_instants_as_iso() {
    let bars = bars(&parsed(REMAINS));
    // end_time / weekly_end_time are epoch-ms.
    assert_eq!(
        bars[0].resets_at.as_deref(),
        Some("2026-09-04T15:00:00+00:00")
    );
    assert_eq!(
        bars[1].resets_at.as_deref(),
        Some("2026-09-07T00:00:00+00:00")
    );
}

#[test]
fn the_video_bucket_is_a_row_and_never_a_bar() {
    // `video` shares the account but not the window Claude Code spends, so
    // sourcing bars from it would report headroom the completions don't have.
    let models = parsed(REMAINS);
    let rows = rows(&models);
    assert!(rows.iter().any(|r| r.label == "video"));
    assert!(
        bars(&models)
            .iter()
            .all(|b| b.label == "5h" || b.label == "7d")
    );
    assert_eq!(bars(&models).len(), 2);
}

#[test]
fn rows_state_what_is_left_under_a_remaining_heading() {
    let rows = rows(&parsed(REMAINS));
    assert_eq!(rows[0].kind, StatRowKind::Heading);
    assert_eq!(rows[0].label, "remaining");
    assert_eq!(rows[1].label, "general");
    assert_eq!(rows[1].value, "5h 38%  ·  7d 69%");
}

#[test]
fn a_spent_interval_marks_its_row_danger() {
    let json = r#"{"model_remains":[{"model_name":"general",
        "current_interval_remaining_percent":0,
        "current_weekly_remaining_percent":40}],
        "base_resp":{"status_code":0}}"#;
    let rows = rows(&parsed(json));
    assert_eq!(rows[1].kind, StatRowKind::Danger);
}

#[test]
fn a_lone_bucket_drives_the_bars_even_when_it_is_not_named_general() {
    let json = r#"{"model_remains":[{"model_name":"text",
        "current_interval_remaining_percent":10,
        "current_weekly_remaining_percent":20}],
        "base_resp":{"status_code":0}}"#;
    let bars = bars(&parsed(json));
    assert_eq!(bars.len(), 2);
    assert!((bars[0].pct - 90.0).abs() < f64::EPSILON);
}

#[test]
fn several_buckets_with_no_general_yield_no_bars() {
    // Nothing here says which bucket the completions bill against, and guessing
    // one would publish another product's headroom as this account's.
    let json = r#"{"model_remains":[
        {"model_name":"video","current_interval_remaining_percent":100},
        {"model_name":"music","current_interval_remaining_percent":100}],
        "base_resp":{"status_code":0}}"#;
    let models = parsed(json);
    assert!(bars(&models).is_empty());
    assert_eq!(rows(&models).len(), 3, "heading plus both buckets");
}

#[test]
fn an_absent_percentage_yields_no_bar_rather_than_full_headroom() {
    let json = r#"{"model_remains":[{"model_name":"general",
        "current_weekly_remaining_percent":50}],
        "base_resp":{"status_code":0}}"#;
    let bars = bars(&parsed(json));
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].label, "7d");
}

#[test]
fn an_empty_plan_renders_nothing_rather_than_a_bare_heading() {
    let models = parsed(r#"{"model_remains":[],"base_resp":{"status_code":0}}"#);
    assert!(rows(&models).is_empty());
    assert!(bars(&models).is_empty());
}

#[test]
fn stats_claim_no_plan_label() {
    // The response names no tier; the only "token plan" in it is the endpoint
    // path, and a label built from that puts a word in the vendor's mouth.
    let s = stats(&parsed(REMAINS));
    assert_eq!(s.plan, None);
    assert!(
        s.is_available,
        "a spent window resets — it is not a balance"
    );
    assert!(!s.best_effort, "typed integration, not the generic scanner");
}

// ── Utilization clamp ─────────────────────────────────────────────────────────

#[test]
fn out_of_range_remaining_percentages_clamp_into_the_bar_range() {
    assert_eq!(utilization(Some(120)), Some(0.0));
    assert_eq!(utilization(Some(-20)), Some(100.0));
    assert_eq!(utilization(None), None);
}
