use super::*;

/// The body is DURATION-keyed, never name-keyed, so the mapping is what decides
/// which of clauth's two named slots a window becomes. A 5-hour window and a
/// 7-day one land where a human means, whichever order the server sends them.
#[test]
fn duration_decides_the_slot_whatever_the_position() {
    let body = r#"{
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true, "limit_reached": false,
            "primary_window":   {"used_percent": 40, "limit_window_seconds": 604800, "reset_after_seconds": 0, "reset_at": 1700000000},
            "secondary_window": {"used_percent": 12, "limit_window_seconds": 18000,  "reset_after_seconds": 0, "reset_at": 1700000001}
        }
    }"#;
    let info = map_usage(body, 1_600_000_000).expect("parses");
    assert_eq!(
        info.five_hour.as_ref().map(|w| w.utilization),
        Some(12.0),
        "the 18000s window is the 5h slot even though it arrived SECOND"
    );
    assert_eq!(
        info.seven_day.as_ref().map(|w| w.utilization),
        Some(40.0),
        "and the 604800s one is weekly even though it arrived first"
    );
    assert_eq!(
        info.plan.as_ref().and_then(|p| p.codex_plan.as_deref()),
        Some("pro"),
        "plan_type is authoritative over the id_token claim"
    );
}

/// A window with no usable duration falls back to POSITION — primary is the
/// short one, secondary the long one, which is the layout every observed
/// account has.
#[test]
fn a_window_without_a_duration_takes_its_positional_slot() {
    let body = r#"{
        "rate_limit": {
            "primary_window":   {"used_percent": 7,  "limit_window_seconds": 0},
            "secondary_window": {"used_percent": 63, "limit_window_seconds": 0}
        }
    }"#;
    let info = map_usage(body, 1_600_000_000).expect("parses");
    assert_eq!(info.five_hour.as_ref().map(|w| w.utilization), Some(7.0));
    assert_eq!(info.seven_day.as_ref().map(|w| w.utilization), Some(63.0));
}

/// Two windows naming the SAME slot is the collision case: the first keeps the
/// slot and the second takes the free one, because dropping it would silently
/// lose the tighter of two real limits.
#[test]
fn two_windows_of_one_kind_do_not_overwrite_each_other() {
    let body = r#"{
        "rate_limit": {
            "primary_window":   {"used_percent": 30, "limit_window_seconds": 18000},
            "secondary_window": {"used_percent": 55, "limit_window_seconds": 3600}
        }
    }"#;
    let info = map_usage(body, 1_600_000_000).expect("parses");
    assert_eq!(
        info.five_hour.as_ref().map(|w| w.utilization),
        Some(30.0),
        "the first claimant keeps the slot its duration named"
    );
    assert_eq!(
        info.seven_day.as_ref().map(|w| w.utilization),
        Some(55.0),
        "the second is kept in the free slot rather than dropped"
    );
}

/// The server's `limit_reached` is a HARD verdict and outranks its own
/// percentages: an account blocked at 96% would otherwise sit under every
/// threshold and keep being chosen by the chain.
#[test]
fn a_reached_limit_outranks_the_percentages_it_came_with() {
    let body = r#"{
        "rate_limit": {
            "limit_reached": true,
            "primary_window":   {"used_percent": 12, "limit_window_seconds": 18000},
            "secondary_window": {"used_percent": 96, "limit_window_seconds": 604800}
        },
        "rate_limit_reached_type": {"type": "rate_limit_reached"}
    }"#;
    let info = map_usage(body, 1_600_000_000).expect("parses");
    assert_eq!(
        info.seven_day.as_ref().map(|w| w.utilization),
        Some(100.0),
        "the block lands on the FULLER window"
    );
    assert_eq!(
        info.five_hour.as_ref().map(|w| w.utilization),
        Some(12.0),
        "and is never fabricated onto the other one"
    );
    assert_eq!(
        info.codex_limit_reached.as_deref(),
        Some("rate_limit_reached")
    );
}

/// A block with no window at all must not invent one — there is nothing to
/// attribute it to, and a fabricated window would be a lie the chain acts on.
#[test]
fn a_reached_limit_with_no_windows_invents_nothing() {
    let info =
        map_usage(r#"{"rate_limit": {"limit_reached": true}}"#, 1_600_000_000).expect("parses");
    assert!(info.five_hour.is_none() && info.seven_day.is_none());
}

/// `reset_at` is the server's absolute answer and wins; `reset_after_seconds`
/// is only the fallback, so a clock skew cannot move a reset the server stated.
#[test]
fn the_absolute_reset_wins_over_the_relative_one() {
    let body = r#"{
        "rate_limit": {
            "primary_window": {"used_percent": 1, "limit_window_seconds": 18000, "reset_after_seconds": 999, "reset_at": 1700000000}
        }
    }"#;
    let info = map_usage(body, 1_600_000_000).expect("parses");
    assert_eq!(
        info.five_hour.as_ref().and_then(|w| w.resets_at.as_deref()),
        Some(crate::usage::epoch_secs_to_iso(1_700_000_000)).as_deref()
    );

    let relative = r#"{
        "rate_limit": {
            "primary_window": {"used_percent": 1, "limit_window_seconds": 18000, "reset_after_seconds": 600}
        }
    }"#;
    let info = map_usage(relative, 1_600_000_000).expect("parses");
    assert_eq!(
        info.five_hour.as_ref().and_then(|w| w.resets_at.as_deref()),
        Some(crate::usage::epoch_secs_to_iso(1_600_000_600)).as_deref(),
        "with no absolute answer the relative one is added to now"
    );
}

/// Banked reset credits ride this same body, so reading them costs no extra
/// request. clauth reads the count and never spends one.
#[test]
fn banked_reset_credits_ride_the_same_body() {
    let info = map_usage(
        r#"{"plan_type": "plus", "rate_limit_reset_credits": {"available_count": 3}}"#,
        1_600_000_000,
    )
    .expect("parses");
    assert_eq!(info.codex_reset_credits, Some(3));
}

/// An empty or unknown-shaped body parses to "no reading", never an error the
/// caller would report as a dead account. Only genuinely malformed JSON fails.
#[test]
fn an_unknown_shape_reads_as_no_data_and_bad_json_as_a_parse_error() {
    let info = map_usage("{}", 1_600_000_000).expect("an empty object is a valid body");
    assert!(info.five_hour.is_none() && info.seven_day.is_none());
    assert!(info.plan.as_ref().is_some_and(|p| p.codex_plan.is_none()));

    let info = map_usage(
        r#"{"plan_type": "future_tier", "rate_limit": null}"#,
        1_600_000_000,
    )
    .expect("an unrecognized plan is still a body");
    assert_eq!(
        info.plan.as_ref().and_then(|p| p.codex_plan.as_deref()),
        Some("future_tier"),
        "held verbatim — clauth does not close this set"
    );

    assert!(map_usage("not json", 1_600_000_000).is_err());
}

/// The claude tier enum stays untouched by a codex reading: its labels all
/// spell "Claude <tier>", which would render a ChatGPT plan as a Claude one.
#[test]
fn a_codex_reading_never_claims_a_claude_tier() {
    let info = map_usage(r#"{"plan_type": "pro"}"#, 1_600_000_000).expect("parses");
    let plan = info.plan.expect("a plan block");
    assert_eq!(plan.tier.display(), None, "no fabricated Claude tier");
    assert_eq!(plan.codex_plan.as_deref(), Some("pro"));
}
