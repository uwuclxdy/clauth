#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Throughput store: recording, recency-weighted pace, degraded + rate-limit
//! flags. Each test sandboxes `$HOME` so the cache lands under a tempdir.

use super::*;
use crate::testutil::HomeSandbox;

#[test]
fn records_and_reads_back_pace() {
    let _home = HomeSandbox::new();
    // The store write is a profile cache, gated on the on-disk record.
    crate::testutil::register_names(&["p"]);
    // 1000 output tokens in 2000ms = 500 tok/s.
    record_success(
        &crate::profile::ProfileName::from("p"),
        Some("sonnet"),
        1000,
        2000,
        100,
    );
    let summary = summary(&crate::profile::ProfileName::from("p"), 100);
    assert_eq!(summary.len(), 1);
    let row = &summary[0];
    assert_eq!(row.model, "sonnet");
    assert!((row.tok_s - 500.0).abs() < 0.001, "tok_s = {}", row.tok_s);
    assert_eq!(row.samples, 1);
    assert!(!row.degraded, "one sample is never degraded");
}

#[test]
fn zero_tokens_or_duration_records_nothing() {
    let _home = HomeSandbox::new();
    record_success(&crate::profile::ProfileName::from("p"), None, 0, 2000, 100);
    record_success(&crate::profile::ProfileName::from("p"), None, 1000, 0, 100);
    assert!(summary(&crate::profile::ProfileName::from("p"), 100).is_empty());
}

#[test]
fn degraded_when_recent_pace_far_below_best() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["p"]);
    // Two fast runs set the best, then several slow runs drag the recent average
    // below half of it.
    record_success(
        &crate::profile::ProfileName::from("p"),
        Some("sonnet"),
        1000,
        1000,
        1,
    ); // 1000 tok/s
    record_success(
        &crate::profile::ProfileName::from("p"),
        Some("sonnet"),
        1000,
        1000,
        2,
    ); // 1000 tok/s
    for t in 3..7 {
        record_success(
            &crate::profile::ProfileName::from("p"),
            Some("sonnet"),
            100,
            1000,
            t,
        ); // 100 tok/s
    }
    let row = summary(&crate::profile::ProfileName::from("p"), 10)
        .into_iter()
        .next()
        .expect("one model");
    assert!(
        row.degraded,
        "recent ~100 tok/s vs best 1000 must read as degraded (tok_s={})",
        row.tok_s
    );
}

#[test]
fn rate_limit_recorded_and_expires() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["p"]);
    record_rate_limit(
        &crate::profile::ProfileName::from("p"),
        Some("opus"),
        Some(30),
        1000,
    );

    let recent = summary(&crate::profile::ProfileName::from("p"), 1000 + 60)
        .into_iter()
        .next()
        .expect("model row");
    assert!(recent.rate_limited_recent, "within the recent window");
    assert_eq!(recent.retry_after_s, Some(30));

    let stale = summary(&crate::profile::ProfileName::from("p"), 1000 + 60 * 60)
        .into_iter()
        .next()
        .expect("model row");
    assert!(
        !stale.rate_limited_recent,
        "an hour later is no longer recent"
    );
    assert_eq!(stale.retry_after_s, None, "retry hint hidden once stale");
}

#[test]
fn unspecified_model_keys_under_default() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["p"]);
    record_success(&crate::profile::ProfileName::from("p"), None, 500, 1000, 5);
    let row = summary(&crate::profile::ProfileName::from("p"), 5)
        .into_iter()
        .next()
        .expect("one model");
    assert_eq!(row.model, "default");
}
