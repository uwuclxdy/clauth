#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::harness::Harness;
use crate::testutil::HomeSandbox;

/// A claude leg's cache write admits only a claude profile: a name only the
/// codex roster holds is refused, so no claude-shaped body reaches the cache
/// the codex surfaces publish by name. Both harnesses resolve `<name>`'s
/// profile dir, so the roster is what tells the two apart.
#[test]
fn a_claude_leg_refuses_a_codex_only_name() {
    let _home = HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx"]);
    let name = ProfileName::from("cx");
    write_profile_cache_for(
        Harness::Claude,
        &name,
        USAGE_CACHE_FILE,
        &crate::usage::UsageInfo::default(),
    );
    assert!(
        profile_cache_path(&name, USAGE_CACHE_FILE).is_some_and(|p| !p.exists()),
        "the claude leg must not write a codex profile's cache"
    );
}

/// The reverse: a codex leg's cache write admits only a codex profile, so a
/// claude-only name gets no codex-shaped body in its cache either.
#[test]
fn a_codex_leg_refuses_a_claude_only_name() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["claude-only"]);
    let name = ProfileName::from("claude-only");
    write_profile_cache_for(
        Harness::Codex,
        &name,
        USAGE_CACHE_FILE,
        &crate::usage::UsageInfo::default(),
    );
    assert!(
        profile_cache_path(&name, USAGE_CACHE_FILE).is_some_and(|p| !p.exists()),
        "the codex leg must not write a claude profile's cache"
    );
}

/// The gate's positive controls: each leg still writes its OWN roster's name.
#[test]
fn each_leg_still_writes_its_own_rosters_name() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["claude-native"]);
    crate::testutil::write_codex_roster(&["cx"]);
    for (harness, name) in [
        (Harness::Claude, ProfileName::from("claude-native")),
        (Harness::Codex, ProfileName::from("cx")),
    ] {
        write_profile_cache_for(
            harness,
            &name,
            USAGE_CACHE_FILE,
            &crate::usage::UsageInfo::default(),
        );
        assert!(
            load_profile_cache::<crate::usage::UsageInfo>(&name, USAGE_CACHE_FILE).is_some(),
            "the {harness} leg's own profile still lands"
        );
    }
}

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

fn refusal_at(at_ms: u64, file: &str) -> CreditRefusal {
    CreditRefusal {
        at_ms,
        file: PathBuf::from(file),
        uuid: None,
    }
}

/// A forget from another process that tries to land between a refusal write's
/// read and its write waits for the write, so the record keeps the forget's
/// floor and a refusal from before it never lands. The peer is a second handle
/// on the state lock file, which a flock treats as another process.
#[test]
fn a_forget_from_another_process_waits_out_a_refusal_write() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["b"]);
    let name = ProfileName::from("b");
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("clauth dir");
    let peer = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("the peer's handle");
    let park = credit_refusal_park::arm();
    let writer = {
        let name = name.clone();
        std::thread::spawn(move || write_credit_refusal(&name, &refusal_at(2_000, "/p/a.jsonl")))
    };
    park.parked
        .recv()
        .expect("the write parks between its read and its write");
    let forgot_inside = peer.try_lock().is_ok();
    if forgot_inside {
        forget_credit_refusal(&name, 5_000);
        peer.unlock().expect("the peer releases");
    }
    park.release.send(()).expect("release the write");
    writer.join().expect("the write");
    if !forgot_inside {
        assert!(
            credit_refusal(&name).is_some(),
            "control: the write landed before the forget"
        );
        peer.lock()
            .expect("the peer takes the flock once the write is done");
        forget_credit_refusal(&name, 5_000);
        peer.unlock().expect("the peer releases");
    }
    assert!(
        credit_refusal(&name).is_none(),
        "the forget dropped the refusal"
    );
    write_credit_refusal(&name, &refusal_at(4_000, "/p/a.jsonl"));
    assert!(
        credit_refusal(&name).is_none(),
        "the forget's floor holds against a refusal from before it"
    );
}

/// A record a newer clauth wrote with one more field still reads: its refusal
/// is on record, and the next write keeps its floor.
#[test]
fn a_refusal_record_with_an_unknown_field_still_reads() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["b"]);
    let name = ProfileName::from("b");
    let path = profile_cache_path(&name, CREDIT_REFUSAL_CACHE_FILE).expect("the record's path");
    std::fs::create_dir_all(path.parent().expect("the profile dir")).expect("the profile dir");
    std::fs::write(
        &path,
        r#"{"refusal":{"at_ms":2000,"file":"/p/a.jsonl"},"floor_ms":1000,"newer":true}"#,
    )
    .expect("write the record");
    assert_eq!(credit_refusal(&name), Some(refusal_at(2_000, "/p/a.jsonl")));
    std::fs::write(&path, r#"{"floor_ms":5000,"newer":true}"#).expect("write the record");
    write_credit_refusal(&name, &refusal_at(4_000, "/p/a.jsonl"));
    assert!(
        credit_refusal(&name).is_none(),
        "the floor the newer record carries still holds"
    );
}

/// A refusal stamped like the one on record is no newer: the record keeps the
/// one it holds, its transcript included.
#[test]
fn a_refusal_with_the_stamp_on_record_keeps_the_one_on_record() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["b"]);
    let name = ProfileName::from("b");
    write_credit_refusal(&name, &refusal_at(2_000, "/p/a.jsonl"));
    write_credit_refusal(&name, &refusal_at(2_000, "/p/b.jsonl"));
    assert_eq!(credit_refusal(&name), Some(refusal_at(2_000, "/p/a.jsonl")));
}

/// A hold that shares a tick's `timed_out` flag once a hold of that tick has
/// timed out skips the flock: the write records nothing and the clear leaves
/// the record standing. The control: with the flag down both go through.
#[test]
fn a_timed_out_tick_writes_no_credit_refusal_and_clears_none() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["b"]);
    let name = ProfileName::from("b");
    let timed_out = std::sync::atomic::AtomicBool::new(true);
    let flag_down = std::sync::atomic::AtomicBool::new(false);
    let refusal = refusal_at(2_000, "/p/a.jsonl");
    write_credit_refusal_flagged(&name, &refusal, Some(&timed_out));
    assert_eq!(credit_refusal(&name), None);
    write_credit_refusal_flagged(&name, &refusal, Some(&flag_down));
    assert_eq!(
        credit_refusal(&name),
        Some(refusal.clone()),
        "control: with the flag down the write lands"
    );
    clear_credit_refusal_flagged(&name, 3_000, Some(&timed_out));
    assert_eq!(credit_refusal(&name), Some(refusal));
    clear_credit_refusal_flagged(&name, 3_000, Some(&flag_down));
    assert_eq!(
        credit_refusal(&name),
        None,
        "control: with the flag down the clear lands"
    );
}
