//! Passive codex usage reader over fixture session trees — never the real
//! `~/.codex`. Fixture shape mirrors real rollout JSONLs at codex-cli 0.144.4.

use super::*;
use crate::testutil::{HomeSandbox, set_mtime};

fn token_count_line(ts: &str, five_pct: f64, seven_pct: f64) -> String {
    serde_json::json!({
        "timestamp": ts,
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": { "total_token_usage": { "total_tokens": 42 } },
            "rate_limits": {
                "primary": { "used_percent": five_pct, "window_minutes": 300, "resets_at": 1_900_000_000_u64 },
                "secondary": { "used_percent": seven_pct, "window_minutes": 10080, "resets_at": 1_900_600_000_u64 },
                "plan_type": "pro",
            },
        },
    })
    .to_string()
}

fn token_count_line_for_limit(ts: &str, limit_id: &str, weekly_pct: f64) -> String {
    serde_json::json!({
        "timestamp": ts,
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": { "total_token_usage": { "total_tokens": 42 } },
            "rate_limits": {
                "limit_id": limit_id,
                "primary": { "used_percent": weekly_pct, "window_minutes": 10080, "resets_at": 1_900_600_000_u64 },
                "plan_type": "pro",
            },
        },
    })
    .to_string()
}

fn write_rollout(sessions: &Path, day: &str, name: &str, lines: &[String]) -> PathBuf {
    let dir = sessions.join(day);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

#[test]
fn reads_the_newest_snapshot_from_the_newest_file() {
    let sandbox = HomeSandbox::new();
    let sessions = sandbox.home().join(".codex/sessions");
    write_rollout(
        &sessions,
        "2026/07/15",
        "rollout-old.jsonl",
        &[token_count_line("2026-07-15T10:00:00Z", 10.0, 5.0)],
    );
    write_rollout(
        &sessions,
        "2026/07/16",
        "rollout-new.jsonl",
        &[
            token_count_line("2026-07-16T01:00:00Z", 40.0, 12.0),
            token_count_line("2026-07-16T02:00:00Z", 55.0, 13.0),
        ],
    );

    let SnapshotOutcome::Snapshot(snap) = read_latest_snapshot_in(&sessions) else {
        panic!("expected a snapshot");
    };
    let five = snap.info.five_hour.expect("5h window");
    assert!(
        (five.utilization - 55.0).abs() < f64::EPSILON,
        "newest line wins"
    );
    assert!(
        five.resets_at.unwrap().contains('T'),
        "resets_at mapped to ISO"
    );
    assert!((snap.info.seven_day.unwrap().utilization - 13.0).abs() < f64::EPSILON);
    assert!(snap.snapshot_at_ms.is_some());
}

#[test]
fn model_specific_limiter_does_not_override_account_wide_codex_usage() {
    let sandbox = HomeSandbox::new();
    let sessions = sandbox.home().join(".codex/sessions");
    write_rollout(
        &sessions,
        "2026/07/18",
        "rollout-mixed-limiters.jsonl",
        &[
            token_count_line_for_limit("2026-07-18T01:00:00Z", "codex", 42.0),
            token_count_line_for_limit("2026-07-18T02:00:00Z", "codex_bengalfox", 0.0),
        ],
    );

    let SnapshotOutcome::Snapshot(snap) = read_latest_snapshot_in(&sessions) else {
        panic!("expected the account-wide codex snapshot");
    };
    assert_eq!(
        snap.info.seven_day.as_ref().map(|w| w.utilization),
        Some(42.0),
        "a newer model-specific quota must not replace overall Codex usage"
    );
}

#[test]
fn model_specific_limiter_alone_is_no_usage_data() {
    let sandbox = HomeSandbox::new();
    let sessions = sandbox.home().join(".codex/sessions");
    write_rollout(
        &sessions,
        "2026/07/18",
        "rollout-model-only.jsonl",
        &[token_count_line_for_limit(
            "2026-07-18T02:00:00Z",
            "codex_bengalfox",
            0.0,
        )],
    );

    assert!(
        matches!(read_latest_snapshot_in(&sessions), SnapshotOutcome::NoData),
        "a model-specific bucket must not publish a fake 0% overall snapshot"
    );
}

// A resumed session appends into its START-date directory, so the freshest
// snapshot can live in a weeks-old dir — mtime order must beat date order
// (ccu's 2026-07-12 catch, ported with the reader).
#[test]
fn resumed_session_in_an_old_dir_wins_by_mtime() {
    let sandbox = HomeSandbox::new();
    let sessions = sandbox.home().join(".codex/sessions");
    let old = write_rollout(
        &sessions,
        "2026/06/01",
        "rollout-resumed.jsonl",
        &[token_count_line("2026-07-16T03:00:00Z", 77.0, 20.0)],
    );
    let new = write_rollout(
        &sessions,
        "2026/07/16",
        "rollout-fresh-dir.jsonl",
        &[token_count_line("2026-07-16T01:00:00Z", 10.0, 5.0)],
    );
    let now = std::time::SystemTime::now();
    set_mtime(&new, now - std::time::Duration::from_secs(3600));
    set_mtime(&old, now);

    let SnapshotOutcome::Snapshot(snap) = read_latest_snapshot_in(&sessions) else {
        panic!("expected a snapshot");
    };
    assert!(
        (snap.info.five_hour.unwrap().utilization - 77.0).abs() < f64::EPSILON,
        "the mtime-newest file wins even from an old date dir"
    );
}

// Lenient decode: malformed lines, token_count without rate_limits, non-JSON
// garbage, and an old flat-schema line are all skipped without hiding an
// older good snapshot in the same file.
#[test]
fn skips_malformed_and_schema_drifted_lines() {
    let sandbox = HomeSandbox::new();
    let sessions = sandbox.home().join(".codex/sessions");
    write_rollout(
        &sessions,
        "2026/07/16",
        "rollout-messy.jsonl",
        &[
            token_count_line("2026-07-16T01:00:00Z", 33.0, 8.0),
            // token_count with no rate_limits block (schema drift)
            r#"{"timestamp":"2026-07-16T02:00:00Z","payload":{"type":"token_count","info":{}}}"#
                .to_string(),
            // old flat schema (no payload wrapper)
            r#"{"type":"token_count","rate_limits":{"primary":{"used_percent":99.0}}}"#.to_string(),
            "not json at all {{{ \"token_count\"".to_string(),
        ],
    );

    let SnapshotOutcome::Snapshot(snap) = read_latest_snapshot_in(&sessions) else {
        panic!("expected the older good snapshot");
    };
    assert!(
        (snap.info.five_hour.unwrap().utilization - 33.0).abs() < f64::EPSILON,
        "drifted lines must not hide the older good one"
    );
}

#[test]
fn missing_tree_and_empty_tree_are_quiet_states() {
    let sandbox = HomeSandbox::new();
    let sessions = sandbox.home().join(".codex/sessions");
    assert!(matches!(
        read_latest_snapshot_in(&sessions),
        SnapshotOutcome::Missing
    ));
    std::fs::create_dir_all(sessions.join("2026/07/16")).unwrap();
    assert!(matches!(
        read_latest_snapshot_in(&sessions),
        SnapshotOutcome::NoData
    ));
}

// `.jsonl.zst` files are recognized and skipped (no zstd dep until reality
// produces compressed files — 0/1136 locally at 0.144.4).
#[test]
fn zst_files_are_skipped_not_fatal() {
    let sandbox = HomeSandbox::new();
    let sessions = sandbox.home().join(".codex/sessions");
    let dir = sessions.join("2026/07/16");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("rollout-x.jsonl.zst"), b"\x28\xb5\x2f\xfd fake").unwrap();
    assert!(
        matches!(read_latest_snapshot_in(&sessions), SnapshotOutcome::NoData),
        "a zst-only tree reads as no-data, never an error"
    );

    write_rollout(
        &sessions,
        "2026/07/16",
        "rollout-plain.jsonl",
        &[token_count_line("2026-07-16T01:00:00Z", 21.0, 3.0)],
    );
    assert!(matches!(
        read_latest_snapshot_in(&sessions),
        SnapshotOutcome::Snapshot(_)
    ));
}

// Attribution gate (§0.8.2): events from before the live auth.json changed
// (switch / login / refresh all rewrite it) are never attributed; missing
// timestamps on either side fail closed.
#[test]
fn attribution_gate_is_conservative() {
    assert!(
        attributable(Some(2_000), Some(1_000)),
        "event after auth change"
    );
    assert!(
        attributable(Some(1_000), Some(1_000)),
        "same instant attributes"
    );
    assert!(
        !attributable(Some(500), Some(1_000)),
        "older event never attributes"
    );
    assert!(
        !attributable(None, Some(1_000)),
        "no event timestamp → fail closed"
    );
    assert!(
        !attributable(Some(2_000), None),
        "no live file → fail closed"
    );
}

// CDX-4 §0.16: the limiter's own verdict rides the snapshot into UsageInfo.
#[test]
fn snapshot_carries_the_rate_limit_reached_verdict() {
    let line = serde_json::json!({
        "timestamp": "2026-07-16T08:00:00Z",
        "payload": {
            "type": "token_count",
            "rate_limits": {
                "primary": { "used_percent": 87.0, "resets_at": 1_900_000_000_u64 },
                "rate_limit_reached_type": "primary",
            },
        },
    })
    .to_string();
    let snap = snapshot_from_lines([line.as_str()].into_iter()).expect("snapshot");
    assert_eq!(
        snap.info.codex_rate_limit_reached.as_deref(),
        Some("primary")
    );

    // An empty verdict string is noise, not a verdict.
    let line = serde_json::json!({
        "timestamp": "2026-07-16T08:00:00Z",
        "payload": {
            "type": "token_count",
            "rate_limits": {
                "primary": { "used_percent": 10.0 },
                "rate_limit_reached_type": "",
            },
        },
    })
    .to_string();
    let snap = snapshot_from_lines([line.as_str()].into_iter()).expect("snapshot");
    assert_eq!(snap.info.codex_rate_limit_reached, None);
}

// ---------------------------------------------------------------------------
// Duration-based slot routing (2026-07 OpenAI limiter re-shape: primary became
// the 10080-minute weekly window, the 5h window temporarily gone)
// ---------------------------------------------------------------------------

#[test]
fn weekly_only_primary_routes_to_the_seven_day_slot() {
    // The observed live shape: primary IS the weekly window, secondary null.
    // Positional mapping would publish a 7-day reset under the "5h" label.
    let line = serde_json::json!({
        "timestamp": "2026-07-16T08:00:00Z",
        "payload": {
            "type": "token_count",
            "rate_limits": {
                "primary": { "used_percent": 36.0, "window_minutes": 10080, "resets_at": 1_784_780_156_u64 },
                "secondary": null,
            },
        },
    })
    .to_string();
    let snap = snapshot_from_lines([line.as_str()].into_iter()).expect("snapshot");
    assert!(snap.info.five_hour.is_none(), "no short window exists");
    let weekly = snap.info.seven_day.as_ref().expect("weekly slot filled");
    assert_eq!(weekly.utilization, 36.0);
}

#[test]
fn verdict_remaps_to_the_slot_its_window_landed_in() {
    // A limit hit on the weekly-duration PRIMARY window must publish as
    // "secondary" (the weekly slot) — consumers read "primary" as the 5h slot.
    let line = serde_json::json!({
        "timestamp": "2026-07-16T08:00:00Z",
        "payload": {
            "type": "token_count",
            "rate_limits": {
                "primary": { "used_percent": 100.0, "window_minutes": 10080, "resets_at": 1_784_780_156_u64 },
                "rate_limit_reached_type": "primary",
            },
        },
    })
    .to_string();
    let snap = snapshot_from_lines([line.as_str()].into_iter()).expect("snapshot");
    assert_eq!(
        snap.info.codex_rate_limit_reached.as_deref(),
        Some("secondary"),
        "weekly-window hit publishes as the weekly slot's verdict"
    );

    // Classic shape: a "secondary" hit on the 10080 window stays "secondary".
    let line = token_count_line("2026-07-16T09:00:00Z", 10.0, 100.0).replace(
        r#""plan_type":"pro""#,
        r#""plan_type":"pro","rate_limit_reached_type":"secondary""#,
    );
    let snap = snapshot_from_lines([line.as_str()].into_iter()).expect("snapshot");
    assert_eq!(
        snap.info.codex_rate_limit_reached.as_deref(),
        Some("secondary")
    );
}

#[test]
fn missing_window_minutes_falls_back_to_positional_slots() {
    // Very old codex releases ship no window_minutes — position decides, and
    // the verdict passes through unremapped.
    let line = serde_json::json!({
        "timestamp": "2026-07-16T08:00:00Z",
        "payload": {
            "type": "token_count",
            "rate_limits": {
                "primary": { "used_percent": 42.0 },
                "secondary": { "used_percent": 7.0 },
                "rate_limit_reached_type": "primary",
            },
        },
    })
    .to_string();
    let snap = snapshot_from_lines([line.as_str()].into_iter()).expect("snapshot");
    assert_eq!(
        snap.info.five_hour.as_ref().map(|w| w.utilization),
        Some(42.0)
    );
    assert_eq!(
        snap.info.seven_day.as_ref().map(|w| w.utilization),
        Some(7.0)
    );
    assert_eq!(
        snap.info.codex_rate_limit_reached.as_deref(),
        Some("primary")
    );
}

#[test]
fn same_slot_collision_keeps_both_windows() {
    // Two weekly-duration windows: primary keeps the weekly slot, secondary is
    // forced to the other slot rather than silently dropped.
    let line = serde_json::json!({
        "timestamp": "2026-07-16T08:00:00Z",
        "payload": {
            "type": "token_count",
            "rate_limits": {
                "primary": { "used_percent": 60.0, "window_minutes": 10080 },
                "secondary": { "used_percent": 30.0, "window_minutes": 20160 },
            },
        },
    })
    .to_string();
    let snap = snapshot_from_lines([line.as_str()].into_iter()).expect("snapshot");
    assert_eq!(
        snap.info.seven_day.as_ref().map(|w| w.utilization),
        Some(60.0)
    );
    assert_eq!(
        snap.info.five_hour.as_ref().map(|w| w.utilization),
        Some(30.0)
    );
}
