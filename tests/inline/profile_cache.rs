#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::testutil::HomeSandbox;

/// The defect the guard closes: `UsageInfo` is all-`serde(default)`, so a read
/// of the WRONG cache file — the third-party leg's bytes — deserialized into an
/// all-default value that read as an empty-but-valid answer. A call site
/// pairing the wrong constant with the wrong type now fails the read.
///
/// The first half is the discriminating pin: `ThirdPartyStats`'s bytes parse
/// into a default `UsageInfo` (only `plan: null` overlaps, every other key is
/// unknown-and-ignored), so without the constant↔type guard this returns
/// `Some` and the pin reds. The reverse half fails on its own — the required
/// `is_available`/`rows` fields reject foreign bytes — and is the belt.
#[test]
fn a_mismatched_usage_cache_read_returns_none() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["litellm"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("litellm"),
        THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    assert!(
        load_profile_cache::<crate::usage::UsageInfo>(
            &crate::profile::ProfileName::from("litellm"),
            THIRD_PARTY_CACHE_FILE,
        )
        .is_none(),
        "the wrong file must not parse into an all-default UsageInfo",
    );

    write_profile_cache(
        &crate::profile::ProfileName::from("litellm"),
        USAGE_CACHE_FILE,
        &crate::usage::UsageInfo::default(),
    );
    assert!(
        load_profile_cache::<crate::providers::ThirdPartyStats>(
            &crate::profile::ProfileName::from("litellm"),
            USAGE_CACHE_FILE,
        )
        .is_none(),
        "the wrong file must not parse into a ThirdPartyStats",
    );
}

/// The guard's positive control: a correctly paired read still returns the
/// value. The check must reject the mismatch, never the read.
#[test]
fn a_correctly_paired_read_still_returns_the_value() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["kerry"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("kerry"),
        USAGE_CACHE_FILE,
        &crate::usage::UsageInfo::default(),
    );

    assert!(
        load_profile_cache::<crate::usage::UsageInfo>(
            &crate::profile::ProfileName::from("kerry"),
            USAGE_CACHE_FILE,
        )
        .is_some(),
        "the correct pairing reads through the guard",
    );
    assert!(
        load_profile_cache::<crate::providers::ThirdPartyStats>(
            &crate::profile::ProfileName::from("litellm"),
            THIRD_PARTY_CACHE_FILE,
        )
        .is_none(),
        "an absent foreign file still reads as no cache, guard or not",
    );
}
