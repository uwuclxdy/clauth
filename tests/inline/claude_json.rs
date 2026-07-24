use super::*;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::json;

use crate::testutil::{HomeSandbox, set_mtime};

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).expect("serialize")).expect("write");
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read")).expect("parse")
}

/// Deterministic timestamps so mtime ordering is unambiguous in tests.
fn t(offset: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + offset)
}

#[test]
fn shared_fields_propagate_from_newest_to_others() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(
        &a,
        &json!({"numStartups": 2, "mcpServers": {"x": 1}, "oauthAccount": {"emailAddress": "a@x"}}),
    );
    write_json(
        &b,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "b@x"}}),
    );
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    sync_paths(&[a.clone(), b.clone()]).expect("sync");

    let bj = read_json(&b);
    assert_eq!(bj["numStartups"], json!(2));
    assert_eq!(bj["mcpServers"], json!({"x": 1}));
    assert_eq!(bj["oauthAccount"]["emailAddress"], json!("b@x")); // per-profile identity kept
    assert_eq!(read_json(&a)["oauthAccount"]["emailAddress"], json!("a@x")); // winner not rewritten
}

#[test]
fn per_profile_fields_never_propagate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(
        &a,
        &json!({
            "shared": 1,
            "oauthAccount": {"emailAddress": "a@x"},
            "passesLastSeenRemaining": 99,
            "overageCreditGrantCache": {"a": true}
        }),
    );
    write_json(
        &b,
        &json!({
            "shared": 0,
            "oauthAccount": {"emailAddress": "b@x"},
            "passesLastSeenRemaining": 0,
            "overageCreditGrantCache": {"b": true}
        }),
    );
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    sync_paths(&[a, b.clone()]).expect("sync");

    let bj = read_json(&b);
    assert_eq!(bj["shared"], json!(1));
    assert_eq!(bj["oauthAccount"]["emailAddress"], json!("b@x"));
    assert_eq!(bj["passesLastSeenRemaining"], json!(0));
    assert_eq!(bj["overageCreditGrantCache"], json!({"b": true}));
}

/// A sync rewrites a member by rename, so the replacement inode takes the
/// writer's mode, not the old file's: a plain write reverts a runtime copy to
/// the umask on every tick, whatever the seed wrote. The home file is Claude
/// Code's own and keeps CC's posture — clauth must not chmod it either way.
#[cfg(unix)]
#[test]
fn sync_writes_runtime_copies_owner_only_and_leaves_the_home_file_alone() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let home_file = home.home().join(".claude.json");
    let winner = home.home().join(".clauth/profiles/p1/runtime/.claude.json");
    let loser = home.home().join(".clauth/profiles/p2/runtime/.claude.json");
    for path in [&winner, &loser] {
        #[allow(clippy::expect_used)]
        fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir runtime");
    }
    write_json(&home_file, &json!({"numStartups": 1}));
    write_json(&winner, &json!({"numStartups": 9}));
    write_json(&loser, &json!({"numStartups": 2}));
    fs::set_permissions(&home_file, fs::Permissions::from_mode(0o644)).expect("chmod home");
    set_mtime(&home_file, t(5));
    set_mtime(&winner, t(10));
    set_mtime(&loser, t(1));

    sync_paths(&[home_file.clone(), winner, loser.clone()]).expect("sync");

    let mode = |p: &Path| fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(
        read_json(&loser)["numStartups"],
        json!(9),
        "precondition: the loser was rewritten by this sync"
    );
    assert_eq!(
        mode(&loser),
        0o600,
        "a runtime .claude.json is clauth-owned; mode should be 0o600, got {:#o}",
        mode(&loser),
    );
    assert_eq!(
        read_json(&home_file)["numStartups"],
        json!(9),
        "precondition: the home file was rewritten by this sync"
    );
    assert_eq!(
        mode(&home_file),
        0o644,
        "~/.claude.json is Claude Code's own file; the syncer must not restyle its mode"
    );
}

#[test]
fn account_scoped_model_caches_stay_per_profile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(
        &a,
        &json!({
            "numStartups": 2,
            "orgModelDefaultCache": {"model": "opus"},
            "modelAccessCache": {"opus": true},
            "additionalModelCostsCache": {"opus": 1},
            "additionalModelOptionsCache": {"opus": ["context-1m"]}
        }),
    );
    // b omits two of the caches entirely and carries its own values for the rest.
    write_json(
        &b,
        &json!({
            "numStartups": 1,
            "modelAccessCache": {"sonnet": true},
            "additionalModelCostsCache": {},
        }),
    );
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    sync_paths(&[a, b.clone()]).expect("sync");

    let bj = read_json(&b);
    assert_eq!(bj["numStartups"], json!(2), "shared field still propagates");
    // a's account-scoped model state never bleeds into b
    assert!(
        bj.get("orgModelDefaultCache").is_none(),
        "absent per-profile key must not be injected from the winner"
    );
    assert!(bj.get("additionalModelOptionsCache").is_none());
    assert_eq!(bj["modelAccessCache"], json!({"sonnet": true}));
    assert_eq!(bj["additionalModelCostsCache"], json!({}));
}

/// `primaryApiKey` is a raw `/login`-managed API key Claude Code stores in
/// `.claude.json`; `customApiKeyResponses` is its per-key approval ledger. Both
/// are account-scoped, so a sync must never carry one profile's into another.
#[test]
fn the_login_managed_api_key_never_propagates() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(
        &a,
        &json!({
            "numStartups": 2,
            "primaryApiKey": "sk-ant-AAAA-a-account-key",
            "customApiKeyResponses": {"approved": ["hash-a"], "rejected": []}
        }),
    );
    // b holds its own key, and would otherwise inherit a's.
    write_json(
        &b,
        &json!({"numStartups": 1, "primaryApiKey": "sk-ant-BBBB-b-account-key"}),
    );
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    sync_paths(&[a, b.clone()]).expect("sync");

    let bj = read_json(&b);
    assert_eq!(bj["numStartups"], json!(2), "shared field still propagates");
    assert_eq!(
        bj["primaryApiKey"],
        json!("sk-ant-BBBB-b-account-key"),
        "one account's raw /login key must never reach another profile"
    );
    assert!(
        bj.get("customApiKeyResponses").is_none(),
        "an absent per-profile key must not be injected from the winner: {bj}"
    );
}

#[test]
fn shared_key_absent_in_winner_is_removed_from_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(&a, &json!({"numStartups": 2}));
    write_json(
        &b,
        &json!({"numStartups": 1, "staleFeature": true, "oauthAccount": {"e": "b"}}),
    );
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    sync_paths(&[a, b.clone()]).expect("sync");

    let bj = read_json(&b);
    assert_eq!(bj["numStartups"], json!(2));
    assert!(
        bj.get("staleFeature").is_none(),
        "a shared key the winner dropped must be removed from the target"
    );
    assert_eq!(bj["oauthAccount"]["e"], json!("b"));
}

#[test]
fn unparseable_file_is_skipped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    let c = tmp.path().join("c.json");
    write_json(&a, &json!({"numStartups": 2, "oauthAccount": {"e": "a"}}));
    fs::write(&b, b"{ partial truncated write").expect("write garbage");
    write_json(&c, &json!({"numStartups": 1, "oauthAccount": {"e": "c"}}));
    set_mtime(&a, t(10));
    set_mtime(&b, t(20)); // newest by mtime but unparseable → skipped
    set_mtime(&c, t(5));

    let before_b = fs::read(&b).expect("read b");
    sync_paths(&[a, b.clone(), c.clone()]).expect("sync");

    // mid-write file never read from nor written to
    assert_eq!(fs::read(&b).expect("read b"), before_b);
    // `a` is the newest parseable member → `c` takes its shared field
    let cj = read_json(&c);
    assert_eq!(cj["numStartups"], json!(2));
    assert_eq!(cj["oauthAccount"]["e"], json!("c"));
}

#[test]
fn newest_mtime_wins_regardless_of_argument_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(&a, &json!({"v": "old"}));
    write_json(&b, &json!({"v": "new"}));
    set_mtime(&a, t(5));
    set_mtime(&b, t(10)); // b newer even though `a` is listed first

    sync_paths(&[a.clone(), b.clone()]).expect("sync");

    assert_eq!(read_json(&a)["v"], json!("new"));
    assert_eq!(read_json(&b)["v"], json!("new"));
}

#[test]
fn converged_target_is_not_rewritten() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(&a, &json!({"numStartups": 5, "oauthAccount": {"e": "a"}}));
    write_json(&b, &json!({"numStartups": 5, "oauthAccount": {"e": "b"}}));
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));
    let before = fs::metadata(&b).unwrap().modified().unwrap();

    sync_paths(&[a, b.clone()]).expect("sync");

    assert_eq!(
        before,
        fs::metadata(&b).unwrap().modified().unwrap(),
        "a target already converged on shared fields must not be rewritten"
    );
}

#[test]
fn single_file_is_noop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    write_json(&a, &json!({"numStartups": 1}));
    let before = fs::read(&a).expect("read");
    sync_paths(std::slice::from_ref(&a)).expect("sync");
    assert_eq!(fs::read(&a).expect("read"), before);
}

// ── strip_home_oauth_account (issue #17 switch-time delete) ───────────────

#[test]
fn strip_home_oauth_account_removes_key_and_preserves_the_rest() {
    let home = HomeSandbox::new();
    let path = home.home().join(".claude.json");
    write_json(
        &path,
        &json!({
            "oauthAccount": {"emailAddress": "stale@x"},
            "numStartups": 3,
            "mcpServers": {"clauth": {"command": "clauth"}},
        }),
    );

    strip_home_oauth_account().expect("strip");

    let after = read_json(&path);
    assert!(
        after.get("oauthAccount").is_none(),
        "stale identity block must be gone"
    );
    assert_eq!(after["numStartups"], json!(3));
    assert_eq!(
        after["mcpServers"],
        json!({"clauth": {"command": "clauth"}})
    );
}

#[test]
fn strip_home_oauth_account_no_op_when_key_absent() {
    let home = HomeSandbox::new();
    let path = home.home().join(".claude.json");
    write_json(&path, &json!({"numStartups": 3}));
    let before_bytes = fs::read(&path).expect("read");
    set_mtime(&path, t(1));
    let before_mtime = fs::metadata(&path).unwrap().modified().unwrap();

    strip_home_oauth_account().expect("strip");

    assert_eq!(
        fs::read(&path).expect("read"),
        before_bytes,
        "a file with no oauthAccount must not be rewritten"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().modified().unwrap(),
        before_mtime,
        "an untouched file must not bump mtime (would make home win the next sync)"
    );
}

#[test]
fn strip_home_oauth_account_skips_unparseable_file() {
    let home = HomeSandbox::new();
    let path = home.home().join(".claude.json");
    fs::write(&path, b"{ mid write, not valid json").expect("write garbage");
    let before = fs::read(&path).expect("read");

    strip_home_oauth_account().expect("strip must not fail on a CC mid-write file");

    assert_eq!(
        fs::read(&path).expect("read"),
        before,
        "an unparseable file must never be clobbered"
    );
}

#[test]
fn strip_home_oauth_account_skips_valid_json_that_is_not_an_object() {
    let home = HomeSandbox::new();
    let path = home.home().join(".claude.json");
    fs::write(&path, b"[]").expect("write non-object json");
    let before = fs::read(&path).expect("read");

    strip_home_oauth_account().expect("strip must not fail on a non-object document");

    assert_eq!(
        fs::read(&path).expect("read"),
        before,
        "a parses-but-not-an-object file must be left untouched"
    );
}

#[test]
fn strip_home_oauth_account_skips_missing_file() {
    let home = HomeSandbox::new();
    let path = home.home().join(".claude.json");

    strip_home_oauth_account().expect("strip must not fail when there is nothing to strip");

    assert!(!path.exists(), "a missing file must never be created");
}

// ── live_oauth_account_uuid: CC's own record of the live login's account ────

#[test]
fn live_oauth_account_uuid_reads_the_home_record() {
    let home = crate::testutil::HomeSandbox::new();
    let path = home.home().join(".claude.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "oauthAccount": { "accountUuid": " uuid-1 ", "emailAddress": "a@b.c" },
            "userID": "x"
        }))
        .expect("ser"),
    )
    .expect("write");
    assert_eq!(home_oauth_account_uuid().as_deref(), Some("uuid-1"));
}

#[test]
fn live_oauth_account_pair_reads_both_halves_from_one_snapshot() {
    let _home = crate::testutil::HomeSandbox::new();
    let path = crate::profile::home_dir().unwrap().join(".claude.json");
    std::fs::write(
        &path,
        r#"{"oauthAccount":{"accountUuid":"uuid-1","emailAddress":"me@example.com"}}"#,
    )
    .unwrap();
    assert_eq!(
        live_oauth_account_pair(),
        Some(("uuid-1".to_string(), Some("me@example.com".to_string()))),
    );
    // Email absent/blank → uuid still anchors, email half is None.
    std::fs::write(
        &path,
        r#"{"oauthAccount":{"accountUuid":"uuid-1","emailAddress":"  "}}"#,
    )
    .unwrap();
    assert_eq!(
        live_oauth_account_pair(),
        Some(("uuid-1".to_string(), None))
    );
    // No uuid → no identity at all, whatever the email says.
    std::fs::write(
        &path,
        r#"{"oauthAccount":{"emailAddress":"me@example.com"}}"#,
    )
    .unwrap();
    assert_eq!(live_oauth_account_pair(), None);
}

#[test]
fn live_oauth_account_uuid_absent_or_blank_reads_none() {
    let home = crate::testutil::HomeSandbox::new();
    // No file at all (a fresh sandbox).
    assert_eq!(home_oauth_account_uuid(), None);
    // File without the block (clauth strips it on switch — issue #17).
    let path = home.home().join(".claude.json");
    std::fs::write(&path, br#"{"userID":"x"}"#).expect("write");
    assert_eq!(home_oauth_account_uuid(), None);
    // Present but blank uuid is shape drift, never an identity.
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({ "oauthAccount": { "accountUuid": "  " } }))
            .expect("ser"),
    )
    .expect("write");
    assert_eq!(home_oauth_account_uuid(), None);
}
