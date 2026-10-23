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
        None,
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
    record_success(
        &crate::profile::ProfileName::from("p"),
        None,
        0,
        2000,
        100,
        None,
    );
    record_success(
        &crate::profile::ProfileName::from("p"),
        None,
        1000,
        0,
        100,
        None,
    );
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
        None,
    ); // 1000 tok/s
    record_success(
        &crate::profile::ProfileName::from("p"),
        Some("sonnet"),
        1000,
        1000,
        2,
        None,
    ); // 1000 tok/s
    for t in 3..7 {
        record_success(
            &crate::profile::ProfileName::from("p"),
            Some("sonnet"),
            100,
            1000,
            t,
            None,
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
        None,
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
    record_success(
        &crate::profile::ProfileName::from("p"),
        None,
        500,
        1000,
        5,
        None,
    );
    let row = summary(&crate::profile::ProfileName::from("p"), 5)
        .into_iter()
        .next()
        .expect("one model");
    assert_eq!(row.model, "default");
}

/// A12 (throughput arm): a delegate spawned under key K1 that finishes after an
/// edit to K2 records nothing — one lost sample, never a sample under the wrong
/// credential.
#[test]
fn a_record_after_the_credential_changed_writes_nothing() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["p"]);
    let name = crate::profile::ProfileName::from("p");

    // K1 on disk; the delegate launch captures this identity.
    let k1 = crate::profile::Profile::new(
        "p".to_string(),
        Some("https://example.com".to_string()),
        Some("k1".to_string()),
    );
    crate::profile::save_profile(&k1).expect("save k1");
    let captured = crate::usage::throughput_identity_for_spawn(&name, &k1);

    // Another process edits the key to K2 before the delegate finishes.
    let k2 = crate::profile::Profile::new(
        "p".to_string(),
        Some("https://example.com".to_string()),
        Some("k2".to_string()),
    );
    crate::profile::save_profile(&k2).expect("save k2");

    record_success(&name, Some("sonnet"), 1000, 2000, 100, captured);
    record_rate_limit(&name, Some("sonnet"), Some(30), 100, captured);

    assert!(
        summary(&name, 100).is_empty(),
        "the stale delegate's records must not land under the new key"
    );
}

/// F8/M8 (throughput positive arm): a delegate spawned and recorded under the
/// SAME keyed profile records one sample — the identity matches, so the sample
/// lands.
#[test]
fn a_record_under_the_same_keyed_profile_lands_one_sample() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["p"]);
    let name = crate::profile::ProfileName::from("p");
    let k1 = crate::profile::Profile::new(
        "p".to_string(),
        Some("https://example.com".to_string()),
        Some("k1".to_string()),
    );
    crate::profile::save_profile(&k1).expect("save k1");
    let captured = crate::usage::throughput_identity_for_spawn(&name, &k1);

    record_success(&name, Some("sonnet"), 1000, 2000, 100, captured);

    assert_eq!(
        summary(&name, 100).len(),
        1,
        "a record under the same keyed profile lands one sample"
    );
}

/// A delegate on a setup-token account with no pair stored records its sample.
/// The spawn has no pair to capture there, so the stored side must not read
/// the setup token into the throughput identity either, though the OAuth half
/// reads it as that account's identity.
#[test]
fn a_spawn_on_a_setup_token_profile_without_a_pair_lands_one_sample() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["p"]);
    let name = crate::profile::ProfileName::from("p");
    let p = crate::profile::Profile::new("p".to_string(), None, None);
    crate::profile::save_profile(&p).expect("save");
    crate::claude::write_session_token(&name, "sk-ant-oat01-setup", crate::usage::now_ms() as i64)
        .expect("mint");
    assert!(
        crate::usage::read_oauth_for_compare(&name)
            .0
            .setup_token
            .is_some(),
        "the OAuth half reads the setup token"
    );

    let captured = crate::usage::throughput_identity_for_spawn(&name, &p);
    record_success(&name, Some("sonnet"), 1000, 2000, 100, captured);

    assert_eq!(
        summary(&name, 100).len(),
        1,
        "the setup-token delegate's record lands one sample"
    );
}

/// R4 (throughput positive arm, production capture): a delegate spawned and
/// recorded under the SAME anchored plain-OAuth profile records one sample —
/// `throughput_identity_for_spawn` captures the anchor digest, which matches
/// the stored identity.
#[test]
fn an_anchored_oauth_spawn_and_record_lands_one_sample() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["p"]);
    let name = crate::profile::ProfileName::from("p");
    let mut p = crate::profile::Profile::new("p".to_string(), None, None);
    p.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "tok-a".to_string(),
            refresh_token: Some("tok-a-rt".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(&p).expect("save");
    crate::usage::seed_login_anchor(
        &name,
        Some(&crate::profile::AccountId::from("u-a".to_string())),
    );

    let captured = crate::usage::throughput_identity_for_spawn(&name, &p);
    assert!(
        captured.is_some(),
        "an anchored OAuth spawn captures a non-default identity"
    );
    record_success(&name, Some("sonnet"), 1000, 2000, 100, captured);

    assert_eq!(
        summary(&name, 100).len(),
        1,
        "the same-credential record lands one sample"
    );
}

/// R3 (throughput stale-config arm, production capture): a delegate spawned
/// under a stale in-memory token records nothing after a peer installed account
/// B — the capture adopts the anchor only when the stored token still equals
/// the in-memory token.
#[test]
fn a_stale_config_oauth_spawn_records_nothing() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["p"]);
    let name = crate::profile::ProfileName::from("p");
    // This process's in-memory profile still holds account A's token.
    let mut stale = crate::profile::Profile::new("p".to_string(), None, None);
    stale.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "tok-a".to_string(),
            refresh_token: Some("tok-a-rt".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(&stale).expect("save stale");
    // A peer installs account B: new token, then anchor B.
    let mut b = stale.clone();
    b.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "tok-b".to_string(),
            refresh_token: Some("tok-b-rt".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(&b).expect("save b");
    crate::usage::seed_login_anchor(
        &name,
        Some(&crate::profile::AccountId::from("u-b".to_string())),
    );

    let captured = crate::usage::throughput_identity_for_spawn(&name, &stale);
    record_success(&name, Some("sonnet"), 1000, 2000, 100, captured);

    assert!(
        summary(&name, 100).is_empty(),
        "the stale delegate's record must not land under account B"
    );
}
