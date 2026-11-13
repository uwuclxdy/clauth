use super::*;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::json;

use crate::testutil::{HomeSandbox, set_mtime};

// Every sync test holds a `HomeSandbox` even though its members live in a
// tempdir: the syncer resolves the operator's own `~/.claude.json` to decide
// which member gets CC's write style, and a test may not read that home.

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

fn mtime_parts(when: SystemTime) -> (i64, u32) {
    let d = when
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("post-epoch");
    (d.as_secs() as i64, d.subsec_nanos())
}

/// A live `clauth start` session's flock-held marker on the sessions dir
/// `runtime::paired_sessions_name` pairs with `copy`'s runtime dir. Keep the
/// returned handle alive for as long as the copy should read as live.
fn arm_live_copy(copy: &Path) -> std::fs::File {
    let runtime_dir = copy.parent().expect("runtime dir");
    let name = runtime_dir
        .file_name()
        .expect("runtime dir name")
        .to_string_lossy()
        .replacen("runtime", "sessions", 1);
    let sessions = runtime_dir.with_file_name(name);
    fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("test-pid")).expect("open pid");
    pid.lock().expect("lock pid");
    pid
}

fn arm_live_copies(copies: &[PathBuf]) -> Vec<std::fs::File> {
    copies.iter().map(|copy| arm_live_copy(copy)).collect()
}

/// Write `path` as a copy clauth wrote: the production write-and-record path,
/// stamped at `when`, so the merge reads it as a recorded copy carrying that
/// write.
fn write_recorded_json(record: &Path, path: &Path, value: &Value, when: SystemTime) {
    let bytes = serde_json::to_vec_pretty(value).expect("serialize");
    crate::jsonsync::write_copy_and_record(
        Some(record),
        path,
        &bytes,
        mtime_parts(when),
        true,
        Some(when),
    )
    .expect("write recorded copy");
}

#[test]
fn shared_fields_propagate_from_newest_to_others() {
    let _home = HomeSandbox::new();
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
    let _home = HomeSandbox::new();
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
/// Code's own: it lands at the writer's umask-moded mode (CC's own posture on
/// this host), and clauth must not chmod it either way.
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
    // This session's copy is clauth's own, so it may decide the pass.
    write_recorded_json(
        &written_record_path().expect("record path"),
        &winner,
        &json!({"numStartups": 9}),
        t(10),
    );
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
    let control = home.home().join("mode-control.json");
    crate::profile::atomic_write(&control, b"{}").expect("write mode control");
    assert_eq!(
        mode(&home_file),
        mode(&control),
        "~/.claude.json lands at the writer's umask-moded mode, not the seed's (got {:#o}, control {:#o})",
        mode(&home_file),
        mode(&control),
    );
}

#[test]
fn account_scoped_model_caches_stay_per_profile() {
    let _home = HomeSandbox::new();
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

/// Two organizations under one account share an `accountUuid`, and
/// `cachedUsageUtilization` is stamped with that uuid alone, so a shared sync
/// would show one profile's 5h/7d usage as every other profile's.
#[test]
fn usage_and_artifact_caches_stay_per_profile() {
    let _home = HomeSandbox::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(
        &a,
        &json!({
            "numStartups": 2,
            "cachedUsageUtilization": {
                "accountUuid": "acct",
                "utilization": {"five_hour": {"utilization": 14}}
            },
            "cachedArtifactRoster": {"accountUuid": "acct", "organizationUuid": "org-a"},
            "artifactRosterDenied": true
        }),
    );
    write_json(
        &b,
        &json!({
            "numStartups": 1,
            "cachedUsageUtilization": {
                "accountUuid": "acct",
                "utilization": {"five_hour": {"utilization": 87}}
            }
        }),
    );
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    sync_paths(&[a, b.clone()]).expect("sync");

    let bj = read_json(&b);
    assert_eq!(bj["numStartups"], json!(2), "shared field still propagates");
    assert_eq!(
        bj["cachedUsageUtilization"]["utilization"]["five_hour"]["utilization"],
        json!(87),
        "the newest file's usage must not overwrite this profile's own"
    );
    assert!(
        bj.get("cachedArtifactRoster").is_none(),
        "absent per-profile key must not be injected from the winner"
    );
    assert!(bj.get("artifactRosterDenied").is_none());
}

#[test]
fn org_scoped_config_and_feature_caches_stay_per_profile() {
    let _home = HomeSandbox::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    let keys = [
        "groveConfigCache",
        "promoStartupStatusCache",
        "metricsStatusCacheByPrincipal",
        "clientDataCacheSlots",
        "cachedGrowthBookFeatures",
        "cachedGrowthBookFeaturesAt",
        "cachedExperimentFeatures",
        "cachedExperimentData",
        "additionalModelOptionsAnsweredAt",
        "lastSeenOrgDefaultUpdatedAt",
        "clientDataCache",
        "metricsStatusCache",
        "githubWebConnectionStatusCache",
        "autoCompactWindowsCache",
        "startupPrefetchedAt",
    ];
    let mut winner = json!({"numStartups": 2});
    for key in keys {
        winner[key] = json!("a");
    }
    write_json(&a, &winner);
    write_json(&b, &json!({"numStartups": 1, "groveConfigCache": "b"}));
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    sync_paths(&[a, b.clone()]).expect("sync");

    let bj = read_json(&b);
    assert_eq!(bj["numStartups"], json!(2), "shared field still propagates");
    assert_eq!(bj["groveConfigCache"], json!("b"));
    for key in &keys[1..] {
        assert!(
            bj.get(*key).is_none(),
            "{key} must not be injected from the winner"
        );
    }
}

/// `primaryApiKey` is a raw `/login`-managed API key Claude Code stores in
/// `.claude.json`; `customApiKeyResponses` is its per-key approval ledger. Both
/// are account-scoped, so a sync must never carry one profile's into another.
#[test]
fn the_login_managed_api_key_never_propagates() {
    let _home = HomeSandbox::new();
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
    let _home = HomeSandbox::new();
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
    let _home = HomeSandbox::new();
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
    let _home = HomeSandbox::new();
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
    let _home = HomeSandbox::new();
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
    let _home = HomeSandbox::new();
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

    strip_home_oauth_account();
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

    strip_home_oauth_account();

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

    strip_home_oauth_account();

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

    strip_home_oauth_account();

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

    strip_home_oauth_account();

    assert!(!path.exists(), "a missing file must never be created");
}

/// Regression for a break that compiles clean: every session now runs out of its
/// own `runtime-<sid>/`, so a discovery hardcoded to `<profile>/runtime` finds
/// nothing and cross-profile `.claude.json` reconciliation silently dies for
/// every live session. Isolated copies stay excluded, as they were.
#[test]
fn known_paths_reach_per_session_copies_and_still_exclude_isolated() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let profiles = home.home().join(".clauth").join("profiles");
    let legacy = profiles.join("p1").join("runtime").join(".claude.json");
    let session = profiles
        .join("p1")
        .join("runtime-4242-0")
        .join(".claude.json");
    let sibling = profiles
        .join("p2")
        .join("runtime-4242-1")
        .join(".claude.json");
    let isolated = profiles
        .join("p1")
        .join("runtime-isolated-4242-2")
        .join(".claude.json");
    for path in [&global, &legacy, &session, &sibling, &isolated] {
        fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        write_json(path, &json!({}));
    }
    // Each of the three shared copies is a live session's own tree; the
    // isolated copy has no live session armable through this walk at all.
    let _live_legacy = arm_live_copy(&legacy);
    let _live_session = arm_live_copy(&session);
    let _live_sibling = arm_live_copy(&sibling);

    let paths = known_paths().expect("known paths");

    assert!(
        paths.contains(&global),
        "the global file is always a member"
    );
    assert!(
        paths.contains(&session),
        "a live session's own runtime-<sid> copy must be a sync member"
    );
    assert!(
        paths.contains(&sibling),
        "another profile's session copy must be a member too"
    );
    assert!(
        paths.contains(&legacy),
        "a legacy unsuffixed runtime/ copy must stay a member"
    );
    assert!(
        !paths.contains(&isolated),
        "an isolated per-session copy must not be a sync member"
    );
    assert_eq!(paths.len(), 4, "no member beyond those four: {paths:#?}");
}

// ── credit-billed model families (#93) ───────────────────────────────────────
//
// Claude Code bills Fable against usage credits always, and every family its
// cached `tengu_calm_seal` flag lists (CC 2.1.302 `X6`/`ike`, compared
// case-insensitively). The set is lowercase, Fable first, deduped.

#[test]
fn credit_billed_families_are_fable_alone_without_a_flag() {
    assert_eq!(credit_billed_families_from(None), vec!["fable".to_string()]);
}

#[test]
fn credit_billed_families_add_every_family_the_flag_lists() {
    assert_eq!(
        credit_billed_families_from(Some(&json!(["Mythos"]))),
        vec!["fable".to_string(), "mythos".to_string()]
    );
    assert_eq!(
        credit_billed_families_from(Some(&json!([
            "fable", "MYTHOS", " mythos ", "", 7, null, "Opus"
        ]))),
        vec![
            "fable".to_string(),
            "mythos".to_string(),
            "opus".to_string()
        ]
    );
}

#[test]
fn a_flag_that_is_not_a_list_adds_nothing() {
    for flag in [
        json!(true),
        json!("Mythos"),
        json!(null),
        json!({"Mythos": true}),
    ] {
        assert_eq!(
            credit_billed_families_from(Some(&flag)),
            vec!["fable".to_string()],
            "flag {flag}"
        );
    }
}

#[test]
fn credit_billed_families_follow_the_cached_flag_on_disk() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("sandboxed home").join(".claude.json");
    assert_eq!(
        credit_billed_families(),
        vec!["fable".to_string()],
        "no file"
    );

    write_json(
        &path,
        &json!({"cachedGrowthBookFeatures": {"tengu_calm_seal": ["Mythos"]}, "numStartups": 3}),
    );
    assert_eq!(
        credit_billed_families(),
        vec!["fable".to_string(), "mythos".to_string()]
    );

    // CC rewrites its flag cache: the next read follows it.
    write_json(
        &path,
        &json!({"cachedGrowthBookFeatures": {"tengu_calm_seal": ["Mythos", "Opus"]}}),
    );
    set_mtime(
        &path,
        std::time::SystemTime::now() + Duration::from_secs(10),
    );
    assert_eq!(
        credit_billed_families(),
        vec![
            "fable".to_string(),
            "mythos".to_string(),
            "opus".to_string()
        ]
    );

    fs::write(&path, b"{\"cachedGrowthBookFeatures\": {\"tengu_calm").expect("write");
    set_mtime(
        &path,
        std::time::SystemTime::now() + Duration::from_secs(20),
    );
    // A file CC is caught mid-writing is skipped like the sync engine skips it:
    // the last good parse still answers, so the set never flaps for a tick.
    assert_eq!(
        credit_billed_families(),
        vec![
            "fable".to_string(),
            "mythos".to_string(),
            "opus".to_string()
        ],
        "a torn read keeps the last good flag"
    );

    // Once the file parses again, its own flag answers.
    write_json(
        &path,
        &json!({"cachedGrowthBookFeatures": {"tengu_calm_seal": []}}),
    );
    set_mtime(
        &path,
        std::time::SystemTime::now() + Duration::from_secs(30),
    );
    assert_eq!(credit_billed_families(), vec!["fable".to_string()]);
}

#[test]
fn a_failed_read_keeps_the_last_good_flag() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("sandboxed home").join(".claude.json");
    write_json(
        &path,
        &json!({"cachedGrowthBookFeatures": {"tengu_calm_seal": ["Mythos"]}}),
    );
    assert_eq!(
        credit_billed_families(),
        vec!["fable".to_string(), "mythos".to_string()]
    );

    // A read that fails for any reason but absence (a directory here: EISDIR
    // on unix, os error 5 on Windows, where a concurrent rename replace also
    // fails an open) is no evidence the flag changed.
    fs::remove_file(&path).expect("remove");
    fs::create_dir(&path).expect("a directory where the file was");
    assert_eq!(
        credit_billed_families(),
        vec!["fable".to_string(), "mythos".to_string()],
        "a failed read keeps the last good flag"
    );

    // Absence is a real answer: no file, no flag.
    fs::remove_dir(&path).expect("remove the directory");
    assert_eq!(credit_billed_families(), vec!["fable".to_string()]);
}

#[test]
fn a_torn_first_read_with_nothing_parsed_yet_is_fable_alone() {
    let _home = HomeSandbox::new();
    let path = home_dir().expect("sandboxed home").join(".claude.json");
    fs::write(&path, b"{\"cachedGrowthBookFeatures\": {\"tengu_calm").expect("write");
    assert_eq!(credit_billed_families(), vec!["fable".to_string()]);
}

// ── N-wrapper rewrite fan-out (#137) ────────────────────────────────────────
//
// `sync_paths` reads every member, picks the newest winner and rewrites each
// stale copy. With no cross-process lock, N live `clauth start` wrappers
// reconciling the same `$HOME/.claude.json` event all read before any of them
// writes, so each stale copy is rewritten N times. The interleave hook holds
// every reader at "all members read, nothing written" to make that race
// deterministic: three threads each run the uncached reconcile core, each opens
// its own lock fd (flock binds to the open file description, so threads model
// processes), and the write counter pins exactly one rewrite per runtime copy.

/// The three shared runtime copies one fixture cycle uses, next to the global.
fn three_runtime_members(home: &HomeSandbox) -> (PathBuf, Vec<PathBuf>, Vec<std::fs::File>) {
    let global = home.home().join(".claude.json");
    let profiles = home.home().join(".clauth/profiles");
    let runtimes: Vec<PathBuf> = ["p1", "p2", "p3"]
        .iter()
        .enumerate()
        .map(|(i, p)| {
            profiles
                .join(p)
                .join(format!("runtime-4242-{i}"))
                .join(".claude.json")
        })
        .collect();
    // Each copy belongs to a live session's own tree, so it is a sync member.
    let live = arm_live_copies(&runtimes);
    (global, runtimes, live)
}

#[test]
fn n_wrappers_rewrite_each_stale_copy_once_per_claude_code_write() {
    let home = HomeSandbox::new();
    let (global, runtimes, _live) = three_runtime_members(&home);
    let mut paths = vec![global.clone()];
    paths.extend(runtimes.iter().cloned());

    // All four members converged on the shared field, each carrying its own
    // per-profile identity.
    for (i, path) in std::iter::once(&global).chain(runtimes.iter()).enumerate() {
        fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        write_json(
            path,
            &json!({"numStartups": 1, "oauthAccount": {"emailAddress": format!("p{i}@x")}}),
        );
    }

    // The "Claude Code write": the global file gains a shared key and the newest
    // mtime, so it is the winner every wrapper must propagate.
    write_json(
        &global,
        &json!({
            "numStartups": 2,
            "freshSharedKey": true,
            "oauthAccount": {"emailAddress": "p0@x"}
        }),
    );
    set_mtime(&global, t(10));
    for (i, path) in runtimes.iter().enumerate() {
        set_mtime(path, t(5 - i as u64));
    }

    // A bounded barrier every reader hits after reading and before writing:
    // all three arrive together when reads are un-serialized (the red), and each
    // proceeds alone after its ~1 s bound when the lock serializes them.
    let arrived = std::sync::Arc::new((std::sync::Mutex::new(0usize), std::sync::Condvar::new()));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_interleave_hook(None);
        }
    }
    crate::jsonsync::reset_write_counts();
    {
        let arrived = std::sync::Arc::clone(&arrived);
        crate::jsonsync::set_interleave_hook(Some(Box::new(move || {
            let (lock, cvar) = &*arrived;
            let mut count = lock.lock().unwrap_or_else(|p| p.into_inner());
            *count += 1;
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while *count < 3 {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (guard, _timed_out) = cvar
                    .wait_timeout(count, remaining)
                    .unwrap_or_else(|p| p.into_inner());
                count = guard;
            }
            if *count == 3 {
                cvar.notify_all();
            }
        })));
    }
    let _hook_guard = HookGuard;

    let paths = std::sync::Arc::new(paths);
    let mut handles = Vec::new();
    for _ in 0..3 {
        let paths = std::sync::Arc::clone(&paths);
        handles.push(std::thread::spawn(move || {
            sync_paths(&paths).expect("sync");
        }));
    }
    for handle in handles {
        handle.join().expect("join");
    }

    // Locked: each runtime copy is rewritten exactly once, by the first wrapper
    // to hold the lock; the global winner is never rewritten. Unlocked, all
    // three wrappers rewrite all three copies (3 each) — the red.
    for path in &runtimes {
        assert_eq!(
            crate::jsonsync::write_count(path),
            1,
            "runtime copy {} rewritten {} times, want exactly 1",
            path.display(),
            crate::jsonsync::write_count(path),
        );
    }
    assert_eq!(
        crate::jsonsync::write_count(&global),
        0,
        "the winner must never be rewritten"
    );
    for path in paths.iter() {
        let value = read_json(path);
        assert_eq!(
            value["freshSharedKey"],
            json!(true),
            "every copy must hold the new shared key: {}",
            path.display(),
        );
    }
}

#[test]
fn a_converged_set_with_no_new_write_rewrites_nothing() {
    let home = HomeSandbox::new();
    let (global, runtimes, _live) = three_runtime_members(&home);
    let mut paths = vec![global.clone()];
    paths.extend(runtimes.iter().cloned());

    for (i, path) in std::iter::once(&global).chain(runtimes.iter()).enumerate() {
        fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        write_json(
            path,
            &json!({"numStartups": 1, "oauthAccount": {"emailAddress": format!("p{i}@x")}}),
        );
    }
    // No new mtime: nothing to propagate, so no member is rewritten.
    crate::jsonsync::reset_write_counts();
    sync_paths(&paths).expect("sync");

    for path in &paths {
        assert_eq!(
            crate::jsonsync::write_count(path),
            0,
            "a converged member must not be rewritten: {}",
            path.display(),
        );
    }
}

// ── the sync lock's caller modes + fail-closed shape (#137) ──────────────────

/// Open the sync-lock file on an independent handle and hold the flock — the
/// "second process" stand-in `tests/inline/lock.rs` uses for the state flock.
/// The returned `File` releases on drop.
fn hold_json_sync_lock() -> fs::File {
    let dir = clauth_dir().expect("clauth dir");
    mkdir_700(&dir).expect("mkdir ~/.clauth");
    let holder = open_state_file(&dir.join(JSON_SYNC_LOCK_FILENAME)).expect("open holder handle");
    holder.lock().expect("hold the flock");
    holder
}

/// A fixture with the global as the newest member (a Claude Code write) and the
/// three runtime copies stale, so the next sync would rewrite each runtime once.
fn stale_runtimes_after_a_claude_code_write(
    home: &HomeSandbox,
) -> (PathBuf, Vec<PathBuf>, Vec<PathBuf>, Vec<std::fs::File>) {
    let (global, runtimes, live) = three_runtime_members(home);
    let mut paths = vec![global.clone()];
    paths.extend(runtimes.iter().cloned());
    for (i, path) in std::iter::once(&global).chain(runtimes.iter()).enumerate() {
        fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        write_json(
            path,
            &json!({"numStartups": 1, "oauthAccount": {"emailAddress": format!("p{i}@x")}}),
        );
    }
    write_json(
        &global,
        &json!({
            "numStartups": 2,
            "freshSharedKey": true,
            "oauthAccount": {"emailAddress": "p0@x"}
        }),
    );
    set_mtime(&global, t(10));
    for (i, path) in runtimes.iter().enumerate() {
        set_mtime(path, t(5 - i as u64));
    }
    (global, runtimes, paths, live)
}

#[test]
fn the_watchdog_leg_skips_a_held_sync_lock_without_writing() {
    let home = HomeSandbox::new();
    // A wait regression would park here for the deadline rather than skip; the
    // short override turns that into a fast red instead of a 25 s stall.
    set_json_sync_lock_timeout_override(Some(Duration::from_millis(300)));
    let (_global, _runtimes, paths, _live) = stale_runtimes_after_a_claude_code_write(&home);

    let holder = hold_json_sync_lock();

    crate::jsonsync::reset_write_counts();
    let start = std::time::Instant::now();
    sync_once_try().expect("a held lock is a skip, not an error");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "the watchdog leg must skip at once when the lock is held, waited {elapsed:?}"
    );
    for path in &paths {
        assert_eq!(
            crate::jsonsync::write_count(path),
            0,
            "nothing may be written while the lock is held: {}",
            path.display(),
        );
    }
    assert!(
        LAST_SYNCED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_none(),
        "a skipped pass must not advance LAST_SYNCED (the next event retries)"
    );
    drop(holder);
}

/// The reconcile serializes on the FILE flock, not any in-process lock. A
/// second handle holding the flock blocks the waiting reconcile; swapping the
/// flock for a process-local `static Mutex` would let this worker write while
/// the fd is held and red the "nothing written while held" assertion.
#[test]
fn a_held_sync_lock_blocks_the_reconcile_until_released() {
    let home = HomeSandbox::new();
    let (_global, runtimes, paths, _live) = stale_runtimes_after_a_claude_code_write(&home);

    let holder = hold_json_sync_lock();
    crate::jsonsync::reset_write_counts();

    let paths = std::sync::Arc::new(paths);
    let worker = std::thread::spawn({
        let paths = std::sync::Arc::clone(&paths);
        move || sync_paths(&paths).expect("sync")
    });

    // Settle: a flock-serialized worker blocks here; a same-process mutex would
    // have written the three tiny runtime copies well inside this window.
    std::thread::sleep(Duration::from_millis(300));
    for path in paths.iter() {
        assert_eq!(
            crate::jsonsync::write_count(path),
            0,
            "nothing may be written while the flock is held: {}",
            path.display(),
        );
    }

    drop(holder);
    worker.join().expect("join");

    for path in &runtimes {
        assert_eq!(
            crate::jsonsync::write_count(path),
            1,
            "each stale runtime is rewritten exactly once after release: {}",
            path.display(),
        );
    }
    for path in paths.iter() {
        let value = read_json(path);
        assert_eq!(
            value["freshSharedKey"],
            json!(true),
            "every copy converges after release: {}",
            path.display(),
        );
    }
}

/// A write landing while the lock is held is propagated on the NEXT pass —
/// the skip leaves [`LAST_SYNCED`] unadvanced, so the fallback tick or the next
/// event retries rather than dropping the write.
#[test]
fn a_write_landing_while_held_propagates_on_the_next_pass() {
    let home = HomeSandbox::new();
    // A try-once regression (waiting on the held lock) fails fast instead of
    // stalling 25 s before the `.expect` below.
    set_json_sync_lock_timeout_override(Some(Duration::from_millis(300)));
    let (global, runtimes, _live) = three_runtime_members(&home);
    let mut paths = vec![global.clone()];
    paths.extend(runtimes.iter().cloned());
    for (i, path) in std::iter::once(&global).chain(runtimes.iter()).enumerate() {
        fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        write_json(
            path,
            &json!({"numStartups": 1, "oauthAccount": {"emailAddress": format!("p{i}@x")}}),
        );
        set_mtime(path, t(1 + i as u64));
    }

    // Converge once so LAST_SYNCED holds a real baseline.
    sync_once().expect("initial convergence");

    let holder = hold_json_sync_lock();

    // A newer Claude Code write lands while the lock is held.
    write_json(
        &global,
        &json!({
            "numStartups": 2,
            "freshSharedKey": true,
            "oauthAccount": {"emailAddress": "p0@x"}
        }),
    );
    set_mtime(&global, t(100));

    crate::jsonsync::reset_write_counts();
    sync_once_try().expect("skip while the lock is held");
    for path in &paths {
        assert_eq!(
            crate::jsonsync::write_count(path),
            0,
            "a held lock must not write or advance the cache: {}",
            path.display(),
        );
    }

    drop(holder);
    sync_once_try().expect("propagate once released");
    for path in paths.iter() {
        let value = read_json(path);
        assert_eq!(
            value["freshSharedKey"],
            json!(true),
            "the write that landed while held must propagate on the next pass: {}",
            path.display(),
        );
    }
}

/// A held lock past the (test-shortened) deadline returns the timeout error,
/// writes nothing, and leaves [`LAST_SYNCED`] unadvanced so the caller retries.
#[test]
fn a_held_sync_lock_times_out_without_writing_or_advancing() {
    let home = HomeSandbox::new();
    set_json_sync_lock_timeout_override(Some(Duration::from_millis(300)));
    let (_global, _runtimes, paths, _live) = stale_runtimes_after_a_claude_code_write(&home);

    let holder = hold_json_sync_lock();
    crate::jsonsync::reset_write_counts();

    let err = sync_once().expect_err("a held lock past the deadline must time out");
    assert!(
        err.downcast_ref::<JsonSyncLockTimeout>().is_some(),
        "a held flock must surface as JsonSyncLockTimeout, got: {err:#}"
    );
    for path in &paths {
        assert_eq!(
            crate::jsonsync::write_count(path),
            0,
            "a timed-out pass must write nothing: {}",
            path.display(),
        );
    }
    assert!(
        LAST_SYNCED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_none(),
        "a timed-out pass must not advance LAST_SYNCED"
    );
    drop(holder);
}

/// The Drop final flush's bounded wait is a WAIT, not a try-once skip. A
/// contended lock released well inside the bound must still converge; a try-once
/// flush would return at once and lose the exiting session's last changes.
#[test]
fn the_drop_flush_waits_out_a_contended_lock_within_the_bound() {
    let home = HomeSandbox::new();
    set_json_sync_lock_timeout_override(Some(Duration::from_secs(2)));
    let (_global, runtimes, paths, _live) = stale_runtimes_after_a_claude_code_write(&home);

    let holder = hold_json_sync_lock();
    crate::jsonsync::reset_write_counts();

    let paths = std::sync::Arc::new(paths);
    let worker = std::thread::spawn(sync_once);

    // Release the holder mid-wait, well inside the 2 s bound.
    std::thread::sleep(Duration::from_millis(100));
    drop(holder);

    worker
        .join()
        .expect("join")
        .expect("the flush converges once the peer releases");
    for path in &runtimes {
        assert_eq!(
            crate::jsonsync::write_count(path),
            1,
            "each stale runtime is rewritten exactly once after release: {}",
            path.display(),
        );
    }
    for path in paths.iter() {
        let value = read_json(path);
        assert_eq!(
            value["freshSharedKey"],
            json!(true),
            "every copy converges after release: {}",
            path.display(),
        );
    }
}

/// A Claude Code write landing inside the reconcile's read→write window must
/// not be reverted. The reconcile re-stats the winner before writing: when it
/// changed since the read, this pass writes nothing and the next pass converges
/// on the newer bytes.
#[test]
fn a_claude_code_write_inside_the_read_write_window_is_not_reverted() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let runtime = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(runtime.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &runtime,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&runtime, t(5));

    let paths = std::sync::Arc::new(vec![global.clone(), runtime.clone()]);

    let parked = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    crate::jsonsync::set_interleave_hook(Some(Box::new({
        let parked = std::sync::Arc::clone(&parked);
        let release = std::sync::Arc::clone(&release);
        move || {
            let (lock, cvar) = &*parked;
            let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
            *p = true;
            cvar.notify_all();
            drop(p);
            let (lock, cvar) = &*release;
            let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
            while !*r {
                r = cvar.wait(r).unwrap_or_else(|p| p.into_inner());
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_interleave_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    crate::jsonsync::reset_write_counts();
    let reconcile = std::thread::spawn({
        let paths = std::sync::Arc::clone(&paths);
        move || sync_paths(&paths).expect("reconcile")
    });

    // Wait until the reconcile parked after its read, holding the lock.
    {
        let (lock, cvar) = &*parked;
        let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
        while !*p {
            p = cvar.wait(p).unwrap_or_else(|p| p.into_inner());
        }
    }

    // A Claude Code write to the winner lands inside the read→write window.
    write_json(
        &global,
        &json!({
            "numStartups": 3,
            "ccNewKey": true,
            "oauthAccount": {"emailAddress": "g@x"}
        }),
    );
    set_mtime(&global, t(100));

    {
        let (lock, cvar) = &*release;
        let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
        *r = true;
        cvar.notify_all();
    }
    reconcile.join().expect("join reconcile");

    assert_eq!(
        crate::jsonsync::write_count(&runtime),
        0,
        "a winner that changed since the read must not be propagated this pass"
    );

    // The next pass reads the newer winner and converges without reverting it.
    sync_paths(&paths).expect("converge");
    assert_eq!(
        read_json(&global)["ccNewKey"],
        json!(true),
        "the Claude Code write must survive the follow-up pass"
    );
    assert_eq!(
        read_json(&runtime)["numStartups"],
        json!(3),
        "the stale runtime converges on the newer winner"
    );
}

/// The switch strip TRY-ONCEs the sync lock (never blocking a daemon
/// tick or the TUI's UI thread past a short bound), and a strip deferred by a
/// held lock lands on the next reconcile — which holds the lock and honours the
/// durable marker.
#[test]
fn a_held_lock_defers_the_strip_to_the_next_reconcile() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let runtime = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(runtime.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "stale@x"}}),
    );
    write_json(
        &runtime,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "r@x"}}),
    );
    set_mtime(&global, t(5));
    set_mtime(&runtime, t(10));

    let holder = hold_json_sync_lock();
    let start = std::time::Instant::now();
    strip_home_oauth_account();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "the strip must try-once and return at once when the lock is held, waited {elapsed:?}"
    );
    // The strip is deferred, not dropped: home still carries the stale block.
    let still = read_json(&global);
    assert!(
        still.get("oauthAccount").is_some(),
        "while the lock is held the strip is only deferred, not applied"
    );
    drop(holder);

    // The next reconcile (a real wrapper pass) honours the marker under the
    // lock and strips home.
    sync_once().expect("reconcile honours the deferred strip");
    let after = read_json(&global);
    assert!(
        after.get("oauthAccount").is_none(),
        "the deferred strip must land on the next reconcile, got: {after}"
    );
}

// ── strip serializes with the reconcile (#17) ────────────────────────────────

/// A strip racing the reconcile's read→write window: the reconcile read home
/// WITH `oauthAccount`, the strip deleted it, then the reconcile restored the
/// stale block from its snapshot. The strip TRY-ONCEs the sync lock: when the
/// reconcile already holds it, the strip records itself as owed and the
/// reconcile — still under the lock — honours that marker before it finishes,
/// so the block stays gone. The interleave hook orders the strip against the
/// reconcile by channel, never a wall-clock sleep.
#[test]
fn a_reconcile_racing_a_strip_leaves_home_without_oauth_account() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let runtime = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(runtime.parent().expect("has parent")).expect("mkdir");
    // The runtime is the newest member (winner); home carries the outgoing
    // account's oauthAccount the strip must remove.
    write_json(
        &global,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "outgoing@x"}}),
    );
    write_json(
        &runtime,
        &json!({
            "numStartups": 2,
            "freshSharedKey": true,
            "oauthAccount": {"emailAddress": "incoming@x"}
        }),
    );
    set_mtime(&global, t(5));
    set_mtime(&runtime, t(10));

    let paths = std::sync::Arc::new(vec![global.clone(), runtime.clone()]);

    let parked = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    crate::jsonsync::set_interleave_hook(Some(Box::new({
        let parked = std::sync::Arc::clone(&parked);
        let release = std::sync::Arc::clone(&release);
        move || {
            let (lock, cvar) = &*parked;
            let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
            *p = true;
            cvar.notify_all();
            drop(p);
            let (lock, cvar) = &*release;
            let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
            while !*r {
                r = cvar.wait(r).unwrap_or_else(|p| p.into_inner());
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_interleave_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    let reconcile = std::thread::spawn({
        let paths = std::sync::Arc::clone(&paths);
        move || sync_paths(&paths).expect("reconcile")
    });

    // Wait until the reconcile parked after reading home WITH its oauthAccount,
    // holding the sync lock.
    {
        let (lock, cvar) = &*parked;
        let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
        while !*p {
            p = cvar.wait(p).unwrap_or_else(|p| p.into_inner());
        }
    }

    // Run the strip while the reconcile is parked holding the lock: try-once
    // finds the lock held, records the strip as owed, and returns at once — the
    // deterministic ordering the old 200 ms sleep approximated.
    let strip = std::thread::spawn(strip_home_oauth_account);
    strip.join().expect("join strip");

    // Release the reconcile; it writes home's merged bytes, then honours the
    // owed marker and strips the block under the same lock.
    {
        let (lock, cvar) = &*release;
        let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
        *r = true;
        cvar.notify_all();
    }

    reconcile.join().expect("join reconcile");

    let after = read_json(&global);
    assert!(
        after.get("oauthAccount").is_none(),
        "the switch strip must survive the racing reconcile, got: {after}"
    );
}

// ── reconcile write races and the strip's deferral + retry bounds ────────────

/// A Claude Code write to the WINNER landing mid-loop — after an earlier
/// target's rename, before a later one's — must abort the rest of this pass. The
/// winner is re-statted before EVERY target write, not once before the loop;
/// without that, the later target keeps stale bytes with a newer mtime and the
/// next pass picks it as winner and reverts the winner's write.
#[test]
fn a_winner_write_landing_mid_loop_aborts_the_rest_of_the_pass() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    let r2 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-1/.claude.json");
    for p in [&global, &r1, &r2] {
        fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
    }
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    write_json(
        &r2,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r2@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));
    set_mtime(&r2, t(4));

    let paths = std::sync::Arc::new(vec![global.clone(), r1.clone(), r2.clone()]);

    // Park the write loop on the SECOND target: r1 is already written when the
    // test wakes, and the winner write lands before r2's turn.
    let iterations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let parked = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    crate::jsonsync::set_write_loop_hook(Some(Box::new({
        let iterations = std::sync::Arc::clone(&iterations);
        let parked = std::sync::Arc::clone(&parked);
        let release = std::sync::Arc::clone(&release);
        move || {
            let n = iterations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n == 0 {
                return; // first target (r1): let it write
            }
            let (lock, cvar) = &*parked;
            let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
            *p = true;
            cvar.notify_all();
            drop(p);
            let (lock, cvar) = &*release;
            let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
            while !*r {
                r = cvar.wait(r).unwrap_or_else(|p| p.into_inner());
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_write_loop_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    crate::jsonsync::reset_write_counts();
    let reconcile = std::thread::spawn({
        let paths = std::sync::Arc::clone(&paths);
        move || sync_paths(&paths).expect("reconcile")
    });

    // Wait until the loop parked before r2's write.
    {
        let (lock, cvar) = &*parked;
        let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
        while !*p {
            p = cvar.wait(p).unwrap_or_else(|p| p.into_inner());
        }
    }

    // A Claude Code write to the winner lands mid-loop.
    write_json(
        &global,
        &json!({
            "numStartups": 3,
            "ccNewKey": true,
            "oauthAccount": {"emailAddress": "g@x"}
        }),
    );
    set_mtime(&global, t(200_000_000));

    {
        let (lock, cvar) = &*release;
        let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
        *r = true;
        cvar.notify_all();
    }
    reconcile.join().expect("join reconcile");

    assert_eq!(
        crate::jsonsync::write_count(&r2),
        0,
        "a winner write landing mid-loop must abort the rest of this pass (r2 untouched)"
    );
    assert_eq!(
        crate::jsonsync::write_count(&r1),
        1,
        "the target written before the winner write is still rewritten once"
    );
    assert_eq!(
        read_json(&global)["numStartups"],
        json!(3),
        "the winner's Claude Code write survives the pass"
    );
    assert_eq!(
        read_json(&global)["ccNewKey"],
        json!(true),
        "the winner's Claude Code write survives the pass"
    );

    // Fix the mtimes so the follow-up winner is unambiguous, then converge: the
    // CC write is not reverted.
    set_mtime(&global, t(30));
    set_mtime(&r1, t(20));
    set_mtime(&r2, t(10));
    sync_paths(&paths).expect("converge");
    assert_eq!(
        read_json(&r1)["numStartups"],
        json!(3),
        "the stale target written before the CC write converges on it"
    );
    assert_eq!(
        read_json(&r2)["numStartups"],
        json!(3),
        "the untouched target converges on the CC write"
    );
}

/// A strip that could not complete — a torn/unparseable home — keeps (or
/// writes) the pending marker, never clears it.
#[test]
fn a_torn_home_keeps_the_pending_marker_instead_of_clearing_it() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    fs::write(&global, b"{ mid write, not valid json").expect("write garbage");
    let before = fs::read(&global).expect("read");

    strip_home_oauth_account();

    assert!(
        strip_pending_exists(),
        "a torn home must keep the pending marker so the strip is retried"
    );
    assert_eq!(
        fs::read(&global).expect("read"),
        before,
        "an unparseable file must never be clobbered"
    );
}

/// A strip whose home write fails defers with the marker kept: home keeps its
/// `oauthAccount` and the owed strip survives to retry. The fault is home's
/// parent dir refusing new files, so the lock and marker under `~/.clauth`
/// still write and the failure is the home write itself.
#[test]
fn a_failed_home_write_defers_the_strip_and_keeps_the_marker() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    fs::create_dir_all(global.parent().expect("has parent")).expect("mkdir");
    // The strip opens the sync-lock and marker files under `~/.clauth`; create
    // that dir before home turns unwritable so only the home write faults.
    fs::create_dir_all(home.home().join(".clauth")).expect("mkdir ~/.clauth");
    write_json(
        &global,
        &json!({"oauthAccount": {"emailAddress": "stale@x"}, "numStartups": 7}),
    );

    let _unwritable = crate::testutil::UnwritableDir::new(home.home());
    strip_home_oauth_account();

    assert!(
        strip_pending_exists(),
        "a failed home write must keep the pending marker so the strip is retried"
    );
    assert_eq!(
        read_json(&global),
        json!({"oauthAccount": {"emailAddress": "stale@x"}, "numStartups": 7}),
        "home keeps its oauthAccount while the write fails"
    );
}

/// The per-target re-stat (not just the winner's) is pinned. A Claude Code
/// write to a NON-winner target inside the read→write window keeps its newer
/// bytes, not clobbered by the stale merge.
#[test]
fn a_non_winner_rewrite_inside_the_window_is_not_clobbered() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let runtime = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(runtime.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "freshSharedKey": true, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    // clauth built this session's copy, so the Claude Code write landing on it
    // below may later win a pass.
    write_recorded_json(
        &written_record_path().expect("record path"),
        &runtime,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r@x"}}),
        t(5),
    );
    set_mtime(&global, t(10));

    let paths = std::sync::Arc::new(vec![global.clone(), runtime.clone()]);

    let parked = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    crate::jsonsync::set_interleave_hook(Some(Box::new({
        let parked = std::sync::Arc::clone(&parked);
        let release = std::sync::Arc::clone(&release);
        move || {
            let (lock, cvar) = &*parked;
            let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
            *p = true;
            cvar.notify_all();
            drop(p);
            let (lock, cvar) = &*release;
            let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
            while !*r {
                r = cvar.wait(r).unwrap_or_else(|p| p.into_inner());
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_interleave_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    crate::jsonsync::reset_write_counts();
    let reconcile = std::thread::spawn({
        let paths = std::sync::Arc::clone(&paths);
        move || sync_paths(&paths).expect("reconcile")
    });

    // Wait until the reconcile parked after its read, holding the sync lock.
    {
        let (lock, cvar) = &*parked;
        let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
        while !*p {
            p = cvar.wait(p).unwrap_or_else(|p| p.into_inner());
        }
    }

    // A Claude Code write to the NON-winner target lands inside the window.
    write_json(
        &runtime,
        &json!({"numStartups": 3, "ccNewKey": true, "oauthAccount": {"emailAddress": "r@x"}}),
    );
    set_mtime(&runtime, t(100));

    {
        let (lock, cvar) = &*release;
        let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
        *r = true;
        cvar.notify_all();
    }
    reconcile.join().expect("join reconcile");

    assert_eq!(
        crate::jsonsync::write_count(&runtime),
        0,
        "a non-winner CC rewrote inside the window keeps its newer bytes"
    );
    assert_eq!(
        read_json(&runtime)["ccNewKey"],
        json!(true),
        "the newer bytes survive"
    );
    assert_eq!(
        read_json(&runtime)["numStartups"],
        json!(3),
        "the newer bytes survive"
    );

    // The surviving write is the newest member now, so the next pass converges
    // on it rather than reverting it.
    sync_paths(&paths).expect("converge");
    assert_eq!(
        read_json(&global)["numStartups"],
        json!(3),
        "the surviving write propagates on the next pass"
    );
}

/// The marker clear removes only the marker it read (compare-and-delete on
/// its stamp). A newer deferral that rewrote the marker during the holder's
/// strip-to-clear window survives, because the clear compares against the stamp
/// read BEFORE the strip.
#[test]
fn a_marker_clear_removes_only_the_marker_it_read() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    fs::create_dir_all(global.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"oauthAccount": {"emailAddress": "stale@x"}, "numStartups": 7}),
    );

    // The holder (a reconcile) reads this marker before its strip.
    mark_strip_pending().expect("mark");
    let first = fs::read(strip_pending_path().expect("clauth dir")).expect("read stamp");

    // A concurrent deferral rewrites the marker in the strip-to-clear window, as
    // a switch whose try-once found the lock held would.
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            set_strip_honour_hook(None);
        }
    }
    let hook_guard = HookGuard;
    set_strip_honour_hook(Some(Box::new(|| {
        mark_strip_pending().expect("newer deferral");
    })));

    honor_pending_strip();
    drop(hook_guard);

    assert!(
        strip_pending_exists(),
        "a newer deferral's marker must survive the holder's clear"
    );
    let second = fs::read(strip_pending_path().expect("clauth dir")).expect("read stamp");
    assert_ne!(
        first, second,
        "the surviving marker is the newer deferral's, not the one the holder read"
    );

    // The next honour clears the surviving marker (home is already clean).
    honor_pending_strip();
    assert!(
        !strip_pending_exists(),
        "the newer marker clears on the next honour"
    );
}

/// A strip that keeps failing (a torn home the honour cannot strip) logs
/// once (latch) and stops forcing the full-parse bypass after three consecutive
/// failures — an unchanged member set skips the body until a member's mtime moves.
#[test]
fn a_stuck_strip_stops_forcing_full_parses_after_three_failures() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let runtime = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(runtime.parent().expect("has parent")).expect("mkdir");
    fs::write(&global, b"{ mid write, not valid json").expect("write garbage");
    write_json(
        &runtime,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&runtime, t(5));
    let _live = arm_live_copy(&runtime);

    mark_strip_pending().expect("mark");
    HONOUR_FAILURES.store(0, std::sync::atomic::Ordering::Relaxed);
    HONOUR_FAILURE_LOGGED.store(false, std::sync::atomic::Ordering::Relaxed);

    for _ in 0..3 {
        sync_once_try().expect("pass reaches the body and fails the honour");
    }
    assert_eq!(
        HONOUR_FAILURES.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "three consecutive honour failures"
    );
    assert!(
        HONOUR_FAILURE_LOGGED.load(std::sync::atomic::Ordering::Relaxed),
        "the failure is reported once"
    );

    // The fourth pass, with no member mtime change, is skipped: the bypass
    // stopped forcing a full parse after three failures.
    sync_once_try().expect("pass");
    assert_eq!(
        HONOUR_FAILURES.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "no further full parse (and so no fourth failure) once the bound is reached"
    );

    // A member's mtime moving re-runs the body and retries the honour.
    set_mtime(&runtime, t(100));
    sync_once_try().expect("pass");
    assert_eq!(
        HONOUR_FAILURES.load(std::sync::atomic::Ordering::Relaxed),
        4,
        "a member's mtime moving retries the honour"
    );
}

/// The fast-path bypass is pinned. A pending marker must force the honour
/// even when no member's mtime moved — otherwise a deferral waits indefinitely
/// for a member to change.
#[test]
fn the_fast_path_honours_a_pending_strip_despite_no_mtime_change() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let runtime = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(runtime.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "stale@x"}}),
    );
    // Already converged on the shared field: the initial pass rewrites nothing,
    // so no member's mtime moves and the fast path's baseline really is stale.
    write_json(
        &runtime,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "r@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&runtime, t(5));

    // Converge once so LAST_SYNCED holds a real baseline.
    sync_once().expect("initial convergence");
    assert!(
        read_json(&global).get("oauthAccount").is_some(),
        "convergence keeps home's per-profile block"
    );

    // Mark the strip pending with no member mtime change: the fast path would
    // skip a normal pass, so the bypass must clear LAST_SYNCED.
    mark_strip_pending().expect("mark");

    sync_once_try().expect("honour");
    assert!(
        read_json(&global).get("oauthAccount").is_none(),
        "the bypass must force the honour despite no member mtime change"
    );
    assert!(!strip_pending_exists(), "the honoured marker clears");
}

// ── rename-window races and the strip's re-stat + retry bounds ───────────────

/// A Claude Code write to the WINNER landing between a target's winner
/// re-stat and its rename must not be reverted on the next pass. The written
/// copy's mtime is back-dated to the winner's READ mtime, so it can never
/// outrank its source; without that, the copy (stale bytes, fresh rename mtime)
/// wins the next pass and reverts the winner's write.
#[test]
fn a_winner_write_landing_between_the_restat_and_the_rename_is_not_reverted() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    let r2 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-1/.claude.json");
    for p in [&global, &r1, &r2] {
        fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
    }
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    write_json(
        &r2,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r2@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));
    set_mtime(&r2, t(4));

    let paths = std::sync::Arc::new(vec![global.clone(), r1.clone(), r2.clone()]);

    // Park the write loop AFTER r1's winner/target re-stat and serialize, but
    // BEFORE its rename — the span the per-iteration re-stat cannot see.
    let parked = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::jsonsync::set_pre_rename_hook(Some(Box::new({
        let parked = std::sync::Arc::clone(&parked);
        let release = std::sync::Arc::clone(&release);
        let fired = std::sync::Arc::clone(&fired);
        move || {
            if fired.fetch_add(1, std::sync::atomic::Ordering::Relaxed) != 0 {
                return;
            }
            let (lock, cvar) = &*parked;
            let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
            *p = true;
            cvar.notify_all();
            drop(p);
            let (lock, cvar) = &*release;
            let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
            while !*r {
                r = cvar.wait(r).unwrap_or_else(|p| p.into_inner());
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_pre_rename_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    crate::jsonsync::reset_write_counts();
    let reconcile = std::thread::spawn({
        let paths = std::sync::Arc::clone(&paths);
        move || sync_paths(&paths).expect("reconcile")
    });

    // Wait until the loop parked between the re-stat and r1's rename.
    {
        let (lock, cvar) = &*parked;
        let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
        while !*p {
            p = cvar.wait(p).unwrap_or_else(|p| p.into_inner());
        }
    }

    // A Claude Code write to the winner lands inside the span. Its mtime is
    // newer than the winner snapshot but older than a fresh rename, so the
    // written copy outranks it only if the copy's mtime is NOT back-dated.
    write_json(
        &global,
        &json!({
            "numStartups": 3,
            "ccNewKey": true,
            "oauthAccount": {"emailAddress": "g@x"}
        }),
    );
    set_mtime(&global, t(50));

    {
        let (lock, cvar) = &*release;
        let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
        *r = true;
        cvar.notify_all();
    }
    reconcile.join().expect("join reconcile");

    // Pass 1: the winner's write survives (it is the winner, never rewritten).
    assert_eq!(
        read_json(&global)["numStartups"],
        json!(3),
        "the winner's write survives pass 1"
    );
    assert_eq!(
        read_json(&global)["ccNewKey"],
        json!(true),
        "the winner's write survives pass 1"
    );

    // The next pass must converge on the winner's write, not the stale copy the
    // loop wrote inside the span.
    sync_paths(&paths).expect("converge");
    assert_eq!(
        read_json(&global)["ccNewKey"],
        json!(true),
        "after_second keeps ccNewKey: a written copy never outranks its source"
    );
    assert_eq!(
        read_json(&global)["numStartups"],
        json!(3),
        "after_second keeps the winner's numStartups"
    );
    assert_eq!(
        read_json(&r1)["ccNewKey"],
        json!(true),
        "the stale target converges on the winner's write"
    );
    assert_eq!(
        read_json(&r2)["ccNewKey"],
        json!(true),
        "the untouched target converges on the winner's write"
    );
}

/// The strip's read-modify-write re-stats home immediately before its
/// write; a Claude Code write landing after the parse keeps its bytes and the
/// owed marker survives so the strip is retried.
#[test]
fn a_claude_code_write_inside_the_strips_rmw_keeps_its_bytes_and_the_marker() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    write_json(
        &global,
        &json!({"oauthAccount": {"emailAddress": "stale@x"}, "numStartups": 1}),
    );

    let parked = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    set_strip_rmw_hook(Some(Box::new({
        let parked = std::sync::Arc::clone(&parked);
        let release = std::sync::Arc::clone(&release);
        move || {
            let (lock, cvar) = &*parked;
            let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
            *p = true;
            cvar.notify_all();
            drop(p);
            let (lock, cvar) = &*release;
            let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
            while !*r {
                r = cvar.wait(r).unwrap_or_else(|p| p.into_inner());
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            set_strip_rmw_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    let strip = std::thread::spawn(strip_home_oauth_account);

    // Wait until the strip parked after its read+parse, holding the sync lock.
    {
        let (lock, cvar) = &*parked;
        let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
        while !*p {
            p = cvar.wait(p).unwrap_or_else(|p| p.into_inner());
        }
    }

    // A Claude Code write lands inside the strip's read-modify-write window.
    write_json(&global, &json!({"numStartups": 5, "ccNewKey": true}));

    {
        let (lock, cvar) = &*release;
        let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
        *r = true;
        cvar.notify_all();
    }
    strip.join().expect("join strip");

    assert_eq!(
        read_json(&global)["numStartups"],
        json!(5),
        "the Claude Code write survives the strip's read-modify-write"
    );
    assert_eq!(
        read_json(&global)["ccNewKey"],
        json!(true),
        "the Claude Code write's new keys survive the strip"
    );
    assert!(
        strip_pending_exists(),
        "the marker stays so the deferred strip is retried"
    );
}

/// The daemon tick's honour (`try_honor_pending_strip`) respects the same
/// three-failure bound as the wrappers: after three consecutive failures it
/// stops the per-tick read-modify-write until home moves.
#[test]
fn the_daemon_tick_honour_respects_the_three_failure_bound() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    // A home the strip cannot strip and which does not move: it parses but is
    // not an object, so every honour fails, and its mtime is stable across the
    // calls so the bound (not the re-arm) is what stops the retry.
    fs::write(&global, b"[1, 2, 3]").expect("write non-object home");
    mark_strip_pending().expect("mark");

    HONOUR_FAILURES.store(0, std::sync::atomic::Ordering::Relaxed);
    HONOUR_FAILURE_LOGGED.store(false, std::sync::atomic::Ordering::Relaxed);

    for _ in 0..6 {
        try_honor_pending_strip();
    }

    assert_eq!(
        HONOUR_FAILURES.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "the tick honour stops after three consecutive failures"
    );
    assert!(
        HONOUR_FAILURE_LOGGED.load(std::sync::atomic::Ordering::Relaxed),
        "the failure is reported once"
    );
}

// ── fresh-mtime common path and post-rename writes ──────────────────────────

/// In the common case (the winner held still), a reconcile-written copy
/// keeps the rename's fresh mtime — never back-dated to the winner's READ
/// mtime — so a running Claude Code session whose cache cursor (its last save's
/// wall clock) sits after the winner's write still reloads the change. Claude
/// Code skips a reload when `mtimeMs <= cache.mtime` (measured on the 2.1.309
/// bundle).
#[test]
fn a_reconciled_copy_carries_a_fresh_mtime_when_the_winner_held_still() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));

    // Claude Code's cache cursor: the wall clock of its last save, taken after
    // the winner's write and before clauth's propagation. The copy must land
    // strictly newer than this stamp.
    let cursor = SystemTime::now();
    std::thread::sleep(Duration::from_millis(50));

    let paths = vec![global.clone(), r1.clone()];
    sync_paths(&paths).expect("reconcile");

    let copy_mtime = fs::metadata(&r1)
        .expect("metadata")
        .modified()
        .expect("mtime");
    assert!(
        copy_mtime > cursor,
        "the copy's mtime must exceed the session cache cursor so Claude Code reloads it \
         (copy {copy_mtime:?} <= cursor {cursor:?})"
    );
}

/// A Claude Code write landing on a copy right after clauth's rename is
/// never restamped. The back-date (taken only when the winner moved) must land
/// on clauth's own temp inode before the rename, never on the destination path
/// after it — otherwise that write's fresh mtime is stomped to the winner's old
/// mtime, ties with the winner, and is reverted on the next pass.
#[test]
fn a_claude_code_write_landing_after_the_rename_is_never_restamped() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));

    // Move the winner between the per-target re-stat and the rename so this
    // pass takes the back-date path; only its mtime changes, never its bytes.
    crate::jsonsync::set_pre_rename_hook(Some(Box::new({
        let global = global.clone();
        move || set_mtime(&global, t(50))
    })));
    // Land a Claude Code write on the copy right after clauth's rename.
    crate::jsonsync::set_post_write_hook(Some(Box::new({
        let r1 = r1.clone();
        move || {
            write_json(
                &r1,
                &json!({
                    "numStartups": 2,
                    "ccKey": true,
                    "oauthAccount": {"emailAddress": "r1@x"}
                }),
            );
            set_mtime(&r1, t(60));
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_pre_rename_hook(None);
            crate::jsonsync::set_post_write_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    let paths = vec![global.clone(), r1.clone()];
    sync_paths(&paths).expect("reconcile");

    // The winner is never rewritten: its mtime moved but its bytes are intact.
    assert_eq!(
        read_json(&global)["numStartups"],
        json!(2),
        "the winner is never rewritten"
    );

    // Disarm both seams so the converge pass runs with no planted writes.
    crate::jsonsync::set_pre_rename_hook(None);
    crate::jsonsync::set_post_write_hook(None);

    // The next pass must converge on the copy's own write, not revert it.
    sync_paths(&paths).expect("converge");
    assert_eq!(
        read_json(&r1)["ccKey"],
        json!(true),
        "after_second keeps the Claude Code key: the back-date never restamps a \
         write that landed after the rename"
    );
}

/// On a filesystem with coarse timestamps, a Claude Code write to the winner
/// that lands within one timestamp tick of clauth's copy write carries the
/// copy's exact mtime. The tie must never go to clauth's own echo: the next pass
/// keeps the Claude Code write and propagates it. Without the echo record, ext4
/// reverted that write on every run; the hook pins the tie by stamping the
/// winner with the copy's mtime, so the shape holds on a filesystem whose clock
/// would have split the two writes.
#[test]
fn a_claude_code_write_tying_a_copy_write_on_mtime_wins_the_next_pass() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));

    // Right after clauth renames the copy into place, Claude Code writes the
    // winner within the same timestamp tick: same mtime as the copy.
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::jsonsync::set_post_write_hook(Some(Box::new({
        let global = global.clone();
        let r1 = r1.clone();
        let fired = std::sync::Arc::clone(&fired);
        move || {
            if fired.fetch_add(1, std::sync::atomic::Ordering::Relaxed) != 0 {
                return;
            }
            let copy_mtime = fs::metadata(&r1)
                .expect("copy metadata")
                .modified()
                .expect("copy mtime");
            write_json(
                &global,
                &json!({
                    "numStartups": 3,
                    "ccNewKey": true,
                    "oauthAccount": {"emailAddress": "g@x"}
                }),
            );
            set_mtime(&global, copy_mtime);
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_post_write_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    let paths = vec![global.clone(), r1.clone()];
    sync_paths(&paths).expect("reconcile");
    crate::jsonsync::set_post_write_hook(None);

    assert_eq!(
        fired.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "precondition: clauth wrote the copy exactly once"
    );
    assert_eq!(
        fs::metadata(&global)
            .expect("meta")
            .modified()
            .expect("mtime"),
        fs::metadata(&r1).expect("meta").modified().expect("mtime"),
        "precondition: the winner's write and the copy tie on mtime"
    );

    sync_paths(&paths).expect("converge");
    assert_eq!(
        read_json(&global),
        json!({
            "numStartups": 3,
            "ccNewKey": true,
            "oauthAccount": {"emailAddress": "g@x"}
        }),
        "the Claude Code write survives the tie"
    );
    assert_eq!(
        read_json(&r1),
        json!({
            "numStartups": 3,
            "ccNewKey": true,
            "oauthAccount": {"emailAddress": "r1@x"}
        }),
        "the copy converges on the Claude Code write, keeping its own account"
    );
}

/// The echo record's own edge: a Claude Code write that keeps a copy's size
/// and lands on clauth's own write's mtime matches that write's `(mtime, size)`
/// exactly, so only the written bytes tell it from clauth's echo. The pass is
/// driven down the back-date path for both copies (the winner moves after each
/// temp write and is put back after each rename), so clauth stamps both with
/// the winner's read mtime and all three members tie with no clock involved.
/// Claude Code then rewrites r1 in place at that same mtime: r2 is clauth's
/// echo, the global and r1 are not, and r1 sorts after the global by path.
#[test]
fn a_same_size_claude_code_write_on_clauths_own_mtime_still_wins_the_tie() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    let r2 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-1/.claude.json");
    for p in [&r1, &r2] {
        fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
    }
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    write_json(
        &r2,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r2@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));
    set_mtime(&r2, t(4));

    // Move the winner's mtime after each temp write (so the copy is back-dated
    // to t(10)) and put it back after each rename (so the next target's early
    // re-stat sees an unmoved winner and the pass writes both copies).
    crate::jsonsync::set_post_temp_hook(Some(Box::new({
        let global = global.clone();
        move || set_mtime(&global, t(50))
    })));
    crate::jsonsync::set_post_write_hook(Some(Box::new({
        let global = global.clone();
        move || set_mtime(&global, t(10))
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_post_temp_hook(None);
            crate::jsonsync::set_post_write_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    let paths = vec![global.clone(), r1.clone(), r2.clone()];
    sync_paths(&paths).expect("reconcile");
    crate::jsonsync::set_post_temp_hook(None);
    crate::jsonsync::set_post_write_hook(None);
    let mtime = |p: &Path| fs::metadata(p).expect("meta").modified().expect("mtime");
    assert_eq!(read_json(&r1)["numStartups"], json!(2), "precondition");
    assert_eq!(read_json(&r2)["numStartups"], json!(2), "precondition");
    assert_eq!(mtime(&r1), t(10), "precondition: r1 was back-dated");
    assert_eq!(mtime(&r2), t(10), "precondition: r2 was back-dated");
    assert_eq!(mtime(&global), t(10), "precondition: the winner is back");

    // Claude Code rewrites r1 in place: one digit changes, so the size holds,
    // and the write lands on the mtime clauth's own write of r1 carried.
    let before = fs::read(&r1).expect("read r1");
    write_json(
        &r1,
        &json!({"numStartups": 7, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    assert_eq!(
        fs::read(&r1).expect("read r1").len(),
        before.len(),
        "precondition: the Claude Code write keeps the copy's size"
    );
    set_mtime(&r1, t(10));

    sync_paths(&paths).expect("converge");
    assert_eq!(
        read_json(&r1),
        json!({"numStartups": 7, "oauthAccount": {"emailAddress": "r1@x"}}),
        "the Claude Code write survives the tie"
    );
    assert_eq!(
        read_json(&r2),
        json!({"numStartups": 7, "oauthAccount": {"emailAddress": "r2@x"}}),
        "the sibling copy converges on it"
    );
    assert_eq!(
        read_json(&global),
        json!({"numStartups": 7, "oauthAccount": {"emailAddress": "g@x"}}),
        "the global converges on it"
    );
}

/// A Claude Code write to the WINNER landing between the back-date
/// decision and the temp write must survive the next pass. The temp is written
/// first and the winner re-statted after it, back-dating the temp on its
/// still-open handle only when the winner moved — so a write that lands in that
/// window is seen and the copy never outranks its source. Without that order,
/// the temp's fresh mtime outranks the moved winner and the next pass reverts
/// the winner's write.
#[test]
fn a_winner_write_landing_between_the_decision_and_the_temp_write_is_never_reverted() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    let r2 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-1/.claude.json");
    for p in [&global, &r1, &r2] {
        fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
    }
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    write_json(
        &r2,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r2@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));
    set_mtime(&r2, t(4));

    let paths = std::sync::Arc::new(vec![global.clone(), r1.clone(), r2.clone()]);

    // Park the write loop after the back-date decision and before the temp
    // write — the span the decision cannot retroactively see.
    let parked = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::jsonsync::set_post_temp_hook(Some(Box::new({
        let parked = std::sync::Arc::clone(&parked);
        let release = std::sync::Arc::clone(&release);
        let fired = std::sync::Arc::clone(&fired);
        move || {
            if fired.fetch_add(1, std::sync::atomic::Ordering::Relaxed) != 0 {
                return;
            }
            let (lock, cvar) = &*parked;
            let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
            *p = true;
            cvar.notify_all();
            drop(p);
            let (lock, cvar) = &*release;
            let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
            while !*r {
                r = cvar.wait(r).unwrap_or_else(|p| p.into_inner());
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_post_temp_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    crate::jsonsync::reset_write_counts();
    let reconcile = std::thread::spawn({
        let paths = std::sync::Arc::clone(&paths);
        move || sync_paths(&paths).expect("reconcile")
    });

    // Wait until the loop parked between the decision and the temp write.
    {
        let (lock, cvar) = &*parked;
        let mut p = lock.lock().unwrap_or_else(|p| p.into_inner());
        while !*p {
            p = cvar.wait(p).unwrap_or_else(|p| p.into_inner());
        }
    }

    // A Claude Code write to the winner lands inside the span. Its mtime is
    // newer than the winner snapshot but older than the temp's fresh rename
    // mtime, so the written copy outranks it only if the copy is NOT back-dated.
    write_json(
        &global,
        &json!({
            "numStartups": 3,
            "ccNewKey": true,
            "oauthAccount": {"emailAddress": "g@x"}
        }),
    );
    set_mtime(&global, t(50));

    {
        let (lock, cvar) = &*release;
        let mut r = lock.lock().unwrap_or_else(|p| p.into_inner());
        *r = true;
        cvar.notify_all();
    }
    reconcile.join().expect("join reconcile");

    // Pass 1: the winner's write survives (it is the winner, never rewritten).
    assert_eq!(
        read_json(&global)["numStartups"],
        json!(3),
        "the winner's write survives pass 1"
    );
    assert_eq!(
        read_json(&global)["ccNewKey"],
        json!(true),
        "the winner's write survives pass 1"
    );

    // The next pass must converge on the winner's write, never the stale copy
    // the loop wrote inside the span.
    crate::jsonsync::set_post_temp_hook(None);
    sync_paths(&paths).expect("converge");
    assert_eq!(
        read_json(&global)["ccNewKey"],
        json!(true),
        "after_second keeps ccNewKey: a temp written before the winner re-stat is \
         back-dated when the winner moved, so it never outranks its source"
    );
    assert_eq!(
        read_json(&global)["numStartups"],
        json!(3),
        "after_second keeps the winner's numStartups"
    );
    assert_eq!(
        read_json(&r1)["ccNewKey"],
        json!(true),
        "the first target converges on the winner's write"
    );
    assert_eq!(
        read_json(&r2)["ccNewKey"],
        json!(true),
        "the second target converges on the winner's write"
    );
}

/// On the back-date path (the winner moved since the pass read it), a
/// clauth runtime copy still lands 0o600. The shared back-dating writer must
/// keep the owner-only discipline of `atomic_write_600`, never fall back to a
/// plain umask-moded temp for the one copy that holds clauth's secrets.
#[cfg(unix)]
#[test]
fn a_backdated_runtime_copy_keeps_the_owner_only_mode() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 9, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));

    // Move the winner between the per-target re-stat and the rename so this
    // pass takes the back-date path; only its mtime changes, never its bytes.
    crate::jsonsync::set_pre_rename_hook(Some(Box::new({
        let global = global.clone();
        move || set_mtime(&global, t(50))
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_pre_rename_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    let paths = vec![global.clone(), r1.clone()];
    sync_paths(&paths).expect("reconcile");

    assert_eq!(
        read_json(&r1)["numStartups"],
        json!(9),
        "precondition: the runtime copy was rewritten on the back-date path"
    );
    let mode = fs::metadata(&r1).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "a back-dated runtime copy is still clauth-owned: mode should be 0o600, got {mode:#o}"
    );
}

/// The written-copy record is owner-only and holds no member bytes — only
/// path, mtime (secs + nanos), size, the digest of the bytes clauth wrote, and
/// the carried rank (secs + nanos) of the Claude Code write those bytes carry.
#[cfg(unix)]
#[test]
fn the_written_copy_record_is_owner_only_and_holds_no_member_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));

    sync_paths(&[global, r1]).expect("reconcile");

    let record = home.home().join(".clauth/.claude-json-sync-written.json");
    let meta = fs::metadata(&record).expect("the record exists");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "the record is owner-only"
    );

    let text = String::from_utf8(fs::read(&record).expect("read")).expect("utf-8");
    assert!(
        !text.contains("oauthAccount") && !text.contains("g@x") && !text.contains("r1@x"),
        "the record holds no member bytes"
    );

    let entries: Vec<Value> = serde_json::from_str(&text).expect("record parses");
    assert_eq!(entries.len(), 1, "one copy was written");
    let mut keys: Vec<String> = entries[0]
        .as_object()
        .expect("entry is an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "digest",
            "mtime_nanos",
            "mtime_secs",
            "path",
            "rank_nanos",
            "rank_secs",
            "size"
        ]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<String>>()
    );
}

/// The merge re-reads the member list inside the sync lock, so a pass whose
/// fast-path list was read before a just-started session's job named its tree
/// still keeps that session's record entry (a start records its seed). The
/// pre-lock hook poses the start between the fast path's list and the lock;
/// deleting the in-lock re-read reds this by pruning the entry.
fn a_pass_listing_members_before_a_start_keeps_its_record_entry(pass: fn() -> Result<()>) {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    write_json(&global, &json!({"numStartups": 1}));
    set_mtime(&global, t(1));
    let p1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    let _live1 = arm_live_copy(&p1);
    write_recorded_json(
        &written_record_path().expect("record"),
        &p1,
        &json!({"numStartups": 2}),
        t(10),
    );
    // A session starting between the fast path's list and the lock: its copy is
    // recorded (as a start records its seed) and a job names its tree (so the
    // re-read inside the lock admits it without a marker).
    let p2 = home
        .home()
        .join(".clauth/profiles/p2/runtime-4242-1/.claude.json");
    let tree2 = p2.parent().expect("runtime dir").to_path_buf();
    set_pre_lock_hook(Some(Box::new({
        let home = home.home().to_path_buf();
        let p2 = p2.clone();
        move || {
            write_recorded_json(
                &written_record_path().expect("record"),
                &p2,
                &json!({"numStartups": 3}),
                t(20),
            );
            crate::testutil::write_claude_job(&home, "0add99bb", &tree2);
            crate::testutil::write_live_supervisor(&tree2);
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            set_pre_lock_hook(None);
        }
    }
    let _hook_guard = HookGuard;
    reset_last_synced();

    pass().expect("sync");

    let record = written_record_path().expect("record");
    let entries: Vec<Value> =
        serde_json::from_slice(&fs::read(&record).expect("read record")).expect("record parses");
    let p2_str = p2.to_string_lossy();
    assert!(
        entries.iter().any(|e| e["path"].as_str() == Some(&*p2_str)),
        "the just-started session's record entry must survive the pass: {entries:#?}"
    );
}

#[test]
fn a_pass_that_listed_members_before_a_session_started_keeps_its_record_entry() {
    a_pass_listing_members_before_a_start_keeps_its_record_entry(sync_once);
}

#[test]
fn a_watchdog_pass_that_listed_members_before_a_session_started_keeps_its_record_entry() {
    a_pass_listing_members_before_a_start_keeps_its_record_entry(sync_once_try);
}

/// An unparseable record reads as empty (every member then ranks as a Claude
/// Code write by its own mtime) and a pass that still converges rewrites it
/// parseably with the copy it wrote.
#[test]
fn an_unparseable_record_reads_as_empty_and_is_rewritten() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));

    let record = home.home().join(".clauth/.claude-json-sync-written.json");
    fs::create_dir_all(record.parent().expect("has parent")).expect("mkdir");
    fs::write(&record, b"{ not json").expect("write unparseable record");

    sync_paths(&[global, r1.clone()]).expect("reconcile");

    assert_eq!(
        read_json(&r1)["numStartups"],
        json!(2),
        "the pass still converges on an empty record"
    );
    let entries: Vec<Value> =
        serde_json::from_slice(&fs::read(&record).expect("read")).expect("record now parses");
    assert_eq!(
        entries.len(),
        1,
        "the rewritten record holds the one written copy"
    );
    assert_eq!(entries[0]["path"], json!(r1.to_string_lossy().into_owned()));
}

/// A pass that aborts on a moved winner after writing one copy still
/// persists that copy's record entry — every exit path that wrote something.
#[test]
fn a_pass_aborted_on_a_moved_winner_still_persists_its_prior_write() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    let r2 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-1/.claude.json");
    for p in [&r1, &r2] {
        fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
    }
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    write_json(
        &r2,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r2@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));
    set_mtime(&r2, t(4));

    // After the first copy (r1) is written, move the winner so the second
    // target's winner re-stat aborts the pass; r1's entry must still persist.
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::jsonsync::set_write_loop_hook(Some(Box::new({
        let global = global.clone();
        let fired = std::sync::Arc::clone(&fired);
        move || {
            if fired.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 1 {
                set_mtime(&global, t(50));
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_write_loop_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    sync_paths(&[global, r1.clone(), r2]).expect("reconcile");
    crate::jsonsync::set_write_loop_hook(None);

    assert_eq!(
        fired.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "precondition: the hook ran before both target iterations"
    );

    let record = home.home().join(".clauth/.claude-json-sync-written.json");
    let entries: Vec<Value> =
        serde_json::from_slice(&fs::read(&record).expect("read")).expect("record parses");
    assert_eq!(entries.len(), 1, "only r1 was written before the abort");
    assert_eq!(entries[0]["path"], json!(r1.to_string_lossy().into_owned()));
}

/// A switch's `oauthAccount` strip rewrites home with a fresh mtime. A Claude
/// Code write a runtime copy holds that no pass has propagated yet must survive
/// it: the strip may never make home outrank a newer copy with stale shared
/// fields: without the merge before the strip, the next pass reverts the copy's
/// `ccR1` and `numStartups`.
#[test]
fn a_switch_strip_never_reverts_an_unpropagated_runtime_write() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    // clauth built this session's copy, then Claude Code wrote it.
    write_recorded_json(
        &written_record_path().expect("record path"),
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
        t(5),
    );
    write_json(
        &r1,
        &json!({
            "numStartups": 3,
            "ccR1": true,
            "oauthAccount": {"emailAddress": "r1@x"}
        }),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(20));

    strip_home_oauth_account();
    assert_eq!(
        read_json(&global).get("oauthAccount"),
        None,
        "precondition: the strip landed"
    );

    let paths = vec![global.clone(), r1.clone()];
    sync_paths(&paths).expect("reconcile");
    assert_eq!(
        read_json(&r1),
        json!({
            "numStartups": 3,
            "ccR1": true,
            "oauthAccount": {"emailAddress": "r1@x"}
        }),
        "the runtime copy keeps its Claude Code write"
    );
    assert_eq!(
        read_json(&global),
        json!({"numStartups": 3, "ccR1": true}),
        "home carries the runtime write and no oauthAccount"
    );
}

/// A pass that writes only some members must keep the echo entries of the
/// members it did not write: dropping them lets a later same-tick Claude Code
/// write to the global tie a runtime copy and lose to it on path order, which
/// reverts the global's write.
#[test]
fn a_pass_that_writes_another_member_keeps_the_unwritten_members_entries() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    let r2 = home
        .home()
        .join(".clauth/profiles/p2/runtime-4242-1/.claude.json");
    for p in [&r1, &r2] {
        fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
    }
    write_json(&global, &json!({"numStartups": 2}));
    write_json(&r1, &json!({"numStartups": 1}));
    write_json(&r2, &json!({"numStartups": 2}));
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));
    set_mtime(&r2, t(5));
    let paths = vec![global.clone(), r1.clone(), r2.clone()];
    sync_paths(&paths).expect("pass 1"); // winner global; writes r1 only
    write_json(&r2, &json!({"numStartups": 0}));
    set_mtime(&r2, t(6));
    sync_paths(&paths).expect("pass 2"); // winner r1; writes r2 only
    let record = home.home().join(".clauth/.claude-json-sync-written.json");
    let entries: Vec<Value> =
        serde_json::from_slice(&fs::read(&record).expect("read record")).expect("record parses");
    assert_eq!(entries.len(), 2, "both written members' entries are kept");

    fs::remove_file(&r2).expect("remove r2"); // r2's session ends
    let r1_mtime = fs::metadata(&r1)
        .expect("r1 meta")
        .modified()
        .expect("r1 mtime");
    write_json(&global, &json!({"numStartups": 3, "ccNewKey": true}));
    set_mtime(&global, r1_mtime); // a Claude Code write in r1's mtime tick
    sync_paths(&paths).expect("pass 3");
    assert_eq!(
        read_json(&global),
        json!({"numStartups": 3, "ccNewKey": true}),
        "the Claude Code write to the global survives the tie"
    );
    assert_eq!(
        read_json(&r1),
        json!({"numStartups": 3, "ccNewKey": true}),
        "r1 converges on the global's write"
    );
}

/// A record whose write keeps failing logs one line, and a clean write of the
/// OTHER record never re-arms the failing record's latch. The fault is the
/// record's temp file create (its parent dir refuses new files), never a
/// directory planted at the record path.
#[test]
fn a_record_write_failure_logs_once_per_reconciler() {
    let home = HomeSandbox::new();
    let lines = crate::logline::LogLines::new();
    let _cap = lines.capture_here();

    let bad_dir = home.home().join("bad-records");
    fs::create_dir_all(&bad_dir).expect("mkdir bad-records");
    let _unwritable = crate::testutil::UnwritableDir::new(&bad_dir);
    let bad = bad_dir.join(".claude-json-sync-written.json");
    let good_dir = home.home().join("good-records");
    fs::create_dir_all(&good_dir).expect("mkdir good-records");
    let good = good_dir.join(".settings-sync-written.json");

    let m = home.home().join("m");
    fs::create_dir_all(&m).expect("mkdir m");
    let (a, b, c, d) = (
        m.join("a.json"),
        m.join("b.json"),
        m.join("c.json"),
        m.join("d.json"),
    );
    for i in 0..3u64 {
        write_json(&a, &json!({"v": i, "k": "a"}));
        write_json(&b, &json!({"v": 100 + i}));
        set_mtime(&a, t(100 + 10 * i));
        set_mtime(&b, t(1));
        crate::jsonsync::sync_paths(&[a.clone(), b.clone()], None, Some(&bad), &[], |_| {
            crate::jsonsync::KeyRule::Shared
        })
        .expect("the failing-record pass still converges");

        write_json(&c, &json!({"v": i, "k": "c"}));
        write_json(&d, &json!({"v": 100 + i}));
        set_mtime(&c, t(100 + 10 * i));
        set_mtime(&d, t(1));
        crate::jsonsync::sync_paths(&[c.clone(), d.clone()], None, Some(&good), &[], |_| {
            crate::jsonsync::KeyRule::Shared
        })
        .expect("the clean-record pass converges");
    }

    let failures = lines
        .snapshot()
        .iter()
        .filter(|l| l.contains("written-copy record"))
        .count();
    assert_eq!(
        failures, 1,
        "the failing record logs once; the clean record's write never re-arms its latch"
    );
    assert_eq!(
        read_json(&b),
        json!({"v": 2, "k": "a"}),
        "the failing-record member pair still converged"
    );
    assert_eq!(
        read_json(&d),
        json!({"v": 2, "k": "c"}),
        "the clean-record member pair still converged"
    );
}

/// Fixture for the strip-ordering tests: home holds an `oauthAccount` and
/// older shared fields; one runtime copy holds a newer Claude Code write no
/// pass has propagated yet.
fn strip_order_fixture(home: &HomeSandbox) -> (PathBuf, PathBuf) {
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    // clauth built this session's copy, then Claude Code wrote it.
    write_recorded_json(
        &written_record_path().expect("record path"),
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
        t(5),
    );
    write_json(
        &r1,
        &json!({
            "numStartups": 3,
            "ccR1": true,
            "oauthAccount": {"emailAddress": "r1@x"}
        }),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(20));
    (global, r1)
}

/// The second Claude Code write `strip_order` tests land on the runtime copy.
fn second_runtime_write() -> Value {
    json!({
        "numStartups": 4,
        "ccR1": true,
        "ccR1b": true,
        "oauthAccount": {"emailAddress": "r1@x"}
    })
}

/// Land the second Claude Code write on the runtime copy at a fixed mtime newer
/// than the fixture's but older than any write clauth makes now, so a strip's
/// fresh home write outranks it by mtime on every filesystem.
fn land_second_runtime_write(r1: &Path) {
    write_json(r1, &second_runtime_write());
    set_mtime(r1, t(30));
}

/// After the strip and one more pass, the runtime copy keeps its own second
/// write and home carries it with no `oauthAccount`.
fn assert_second_write_survives(global: &Path, r1: &Path) {
    sync_paths(&[global.to_path_buf(), r1.to_path_buf()]).expect("reconcile");
    assert_eq!(
        read_json(r1),
        second_runtime_write(),
        "the runtime copy keeps its Claude Code write"
    );
    assert_eq!(
        read_json(global),
        json!({"numStartups": 4, "ccR1": true, "ccR1b": true}),
        "home carries the runtime write and no oauthAccount"
    );
}

/// A Claude Code write to a runtime copy landing while a switch strip is
/// rewriting home must survive: the strip's fresh home write never outranks a
/// Claude Code write it did not read.
#[test]
fn a_runtime_write_landing_inside_a_switch_strip_survives() {
    let home = HomeSandbox::new();
    let (global, r1) = strip_order_fixture(&home);
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    set_strip_rmw_hook(Some(Box::new({
        let fired = std::sync::Arc::clone(&fired);
        let r1 = r1.clone();
        move || {
            if fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                land_second_runtime_write(&r1);
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            set_strip_rmw_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    strip_home_oauth_account();
    set_strip_rmw_hook(None);
    assert_eq!(
        fired.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "precondition: the write landed inside the strip"
    );
    assert_eq!(
        read_json(&global).get("oauthAccount"),
        None,
        "precondition: the strip landed"
    );
    assert_second_write_survives(&global, &r1);
}

/// The same through a sync pass honouring a pending strip: a runtime write
/// landing while the honour rewrites home survives the next pass.
#[test]
fn a_runtime_write_landing_inside_an_honoured_strip_survives() {
    let home = HomeSandbox::new();
    let (global, r1) = strip_order_fixture(&home);
    mark_strip_pending().expect("mark the strip owed");
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    set_strip_rmw_hook(Some(Box::new({
        let fired = std::sync::Arc::clone(&fired);
        let r1 = r1.clone();
        move || {
            if fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                land_second_runtime_write(&r1);
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            set_strip_rmw_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    sync_paths(&[global.clone(), r1.clone()]).expect("reconcile and honour");
    set_strip_rmw_hook(None);
    assert_eq!(
        fired.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "precondition: the write landed inside the honoured strip"
    );
    assert_eq!(
        read_json(&global).get("oauthAccount"),
        None,
        "precondition: the honour stripped home"
    );
    assert_second_write_survives(&global, &r1);
}

/// A runtime write landing right after a switch strip rewrote home survives
/// the next pass: the strip's fresh home write never outranks it.
#[test]
fn a_runtime_write_landing_right_after_a_strip_survives() {
    let home = HomeSandbox::new();
    let (global, r1) = strip_order_fixture(&home);
    strip_home_oauth_account();
    land_second_runtime_write(&r1);
    assert_second_write_survives(&global, &r1);
}

/// The daemon tick's honour of a pending strip never reverts an unpropagated
/// runtime write either.
#[test]
fn the_ticks_strip_honour_never_reverts_an_unpropagated_runtime_write() {
    let home = HomeSandbox::new();
    let (global, r1) = strip_order_fixture(&home);
    mark_strip_pending().expect("mark the strip owed");
    try_honor_pending_strip();
    assert!(
        !strip_pending_exists(),
        "precondition: the tick honoured it"
    );
    sync_paths(&[global.clone(), r1.clone()]).expect("reconcile");
    assert_eq!(
        read_json(&r1),
        json!({"numStartups": 3, "ccR1": true, "oauthAccount": {"emailAddress": "r1@x"}}),
        "the runtime copy keeps its Claude Code write"
    );
    assert_eq!(
        read_json(&global),
        json!({"numStartups": 3, "ccR1": true}),
        "home carries the runtime write and no oauthAccount"
    );
}

/// Home holds its own Claude Code write, newer than the runtime copy that won
/// the last pass and not propagated yet, when a strip lands. The strip copies
/// that write forward into its rewrite of home, so the next pass must keep it
/// and propagate it, never revert it to the runtime copy's older write.
fn assert_homes_own_write_survives_a_strip(strip: impl FnOnce()) {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(r1.parent().expect("has parent")).expect("mkdir");
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    // clauth built this session's copy, then Claude Code wrote it.
    write_recorded_json(
        &written_record_path().expect("record path"),
        &r1,
        &json!({"numStartups": 1, "oauthAccount": {"emailAddress": "r1@x"}}),
        t(5),
    );
    write_json(
        &r1,
        &json!({"numStartups": 3, "ccR1": true, "oauthAccount": {"emailAddress": "r1@x"}}),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(20));
    let paths = vec![global.clone(), r1.clone()];
    sync_paths(&paths).expect("first pass");
    assert_eq!(read_json(&global)["numStartups"], json!(3), "precondition");

    // Home's own Claude Code write, newer than the runtime copy's.
    let mut g = read_json(&global);
    g["numStartups"] = json!(5);
    g["ccHome"] = json!(true);
    write_json(&global, &g);
    set_mtime(&global, t(30));

    strip();
    assert_eq!(
        read_json(&global).get("oauthAccount"),
        None,
        "precondition: the strip landed"
    );

    sync_paths(&paths).expect("next pass");
    assert_eq!(
        read_json(&global),
        json!({"numStartups": 5, "ccR1": true, "ccHome": true}),
        "home keeps its own Claude Code write through the strip"
    );
    assert_eq!(
        read_json(&r1),
        json!({
            "numStartups": 5,
            "ccR1": true,
            "ccHome": true,
            "oauthAccount": {"emailAddress": "r1@x"}
        }),
        "the runtime copy converges on home's write"
    );
}

#[test]
fn a_switch_strip_carries_homes_own_unpropagated_write_forward() {
    assert_homes_own_write_survives_a_strip(strip_home_oauth_account);
}

#[test]
fn the_ticks_strip_honour_carries_homes_own_unpropagated_write_forward() {
    assert_homes_own_write_survives_a_strip(|| {
        mark_strip_pending().expect("mark the strip owed");
        try_honor_pending_strip();
        assert!(
            !strip_pending_exists(),
            "precondition: the tick honoured it"
        );
    });
}

/// A pass that skips a copy Claude Code moved mid-loop leaves a sibling copy
/// clauth just wrote (fresh mtime, built from an older winner) beside it; the
/// strip's own record write must keep that sibling's entry, or the sibling's
/// fresh write outranks the moved copy's Claude Code write and reverts it.
#[test]
fn a_strip_keeps_the_record_entries_of_copies_it_did_not_write() {
    let home = HomeSandbox::new();
    let global = home.home().join(".claude.json");
    let r1 = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    let r2 = home
        .home()
        .join(".clauth/profiles/p2/runtime-4242-1/.claude.json");
    for p in [&r1, &r2] {
        fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
    }
    write_json(
        &global,
        &json!({"numStartups": 2, "oauthAccount": {"emailAddress": "g@x"}}),
    );
    write_json(&r1, &json!({"numStartups": 1}));
    // r2 is clauth's own copy, so the Claude Code write the hook lands on it
    // may win a later pass.
    write_recorded_json(
        &written_record_path().expect("record path"),
        &r2,
        &json!({"numStartups": 1}),
        t(5),
    );
    set_mtime(&global, t(10));
    set_mtime(&r1, t(5));

    // Claude Code writes r2 while the pass is writing r1: r2 is skipped as
    // moved, r1 gets a fresh copy of the older winner.
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::jsonsync::set_write_loop_hook(Some(Box::new({
        let fired = std::sync::Arc::clone(&fired);
        let r2 = r2.clone();
        move || {
            if fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                write_json(&r2, &json!({"numStartups": 7, "ccR2": true}));
                set_mtime(&r2, t(15));
            }
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_write_loop_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    let paths = vec![global.clone(), r1.clone(), r2.clone()];
    sync_paths(&paths).expect("pass with a mid-loop write");
    crate::jsonsync::set_write_loop_hook(None);
    assert!(
        fired.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "precondition: the mid-loop write landed"
    );
    assert_eq!(
        read_json(&r2)["ccR2"],
        json!(true),
        "precondition: r2 skipped"
    );

    strip_home_oauth_account();
    sync_paths(&paths).expect("next pass");
    assert_eq!(
        read_json(&r2),
        json!({"numStartups": 7, "ccR2": true}),
        "the moved copy keeps its Claude Code write"
    );
    assert_eq!(
        read_json(&r1),
        json!({"numStartups": 7, "ccR2": true}),
        "the sibling converges on it"
    );
    assert_eq!(
        read_json(&global),
        json!({"numStartups": 7, "ccR2": true}),
        "home converges on it, with no oauthAccount"
    );
}

/// The back-date's remaining job: an UNRECORDED copy built from a winner that
/// moved mid-pass must never outrank that winner's newer write. When the record
/// write fails, the copy ranks by its own mtime, so only the back-date (stamping
/// the temp to the winner's read mtime before the rename) keeps the stale copy
/// below the winner's newer write on the next pass.
#[test]
fn an_unrecorded_copy_from_a_moved_winner_never_outranks_its_newer_write() {
    let home = HomeSandbox::new();
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.json");
    let b = tmp.path().join("b.json");
    write_json(&a, &json!({"numStartups": 2}));
    write_json(&b, &json!({"numStartups": 1}));
    set_mtime(&a, t(10));
    set_mtime(&b, t(5));

    // A record path that cannot be written: the copy below is unrecorded, so it
    // ranks by its own mtime and only the back-date keeps it below the winner.
    let bad_dir = home.home().join("bad-records");
    fs::create_dir_all(&bad_dir).expect("mkdir bad-records");
    let _unwritable = crate::testutil::UnwritableDir::new(&bad_dir);
    let bad = bad_dir.join(".claude-json-sync-written.json");

    // A real Claude Code write lands on the winner after the temp write, before
    // the winner re-stat that decides the back-date.
    crate::jsonsync::set_post_temp_hook(Some(Box::new({
        let a = a.clone();
        move || {
            write_json(&a, &json!({"numStartups": 3, "ccNew": true}));
            set_mtime(&a, t(50));
        }
    })));
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::jsonsync::set_post_temp_hook(None);
        }
    }
    let _hook_guard = HookGuard;

    crate::jsonsync::sync_paths(&[a.clone(), b.clone()], None, Some(&bad), &[], |_| {
        crate::jsonsync::KeyRule::Shared
    })
    .expect("the unrecorded pass still converges");
    drop(_hook_guard);

    // The next pass must converge on the winner's newer write, not the stale
    // unrecorded copy.
    crate::jsonsync::sync_paths(&[a.clone(), b.clone()], None, Some(&bad), &[], |_| {
        crate::jsonsync::KeyRule::Shared
    })
    .expect("second pass");
    assert_eq!(
        read_json(&a),
        json!({"numStartups": 3, "ccNew": true}),
        "the winner's newer write survives the unrecorded copy"
    );
    assert_eq!(
        read_json(&b),
        json!({"numStartups": 3, "ccNew": true}),
        "the unrecorded copy converges on the winner's newer write"
    );
}

/// A `.claude.json` a stray Claude Code wrote into a runtime-named dir no live
/// clauth session holds: one under a profile clauth never made (a stale
/// `CLAUDE_CONFIG_DIR` naming a collected runtime), one beside a sessions dir
/// whose marker no process holds. Both carry a first-start key set, newer than
/// home.
fn plant_dead_runtime_copies(home: &Path) {
    let fresh = json!({"firstStartTime": "2026-10-31T16:24:19.029Z", "userID": "new",
        "machineID": "m", "migrationVersion": 1, "seenNotifications": {}});
    let ghost = home.join(".clauth/profiles/zcode9/runtime-3532172-11/.claude.json");
    fs::create_dir_all(ghost.parent().expect("parent")).expect("mkdir");
    write_json(&ghost, &fresh);
    set_mtime(&ghost, t(20));
    let stale = home.join(".clauth/profiles/cld0/runtime-4242-0/.claude.json");
    fs::create_dir_all(stale.parent().expect("parent")).expect("mkdir");
    fs::create_dir_all(home.join(".clauth/profiles/cld0/sessions-4242-0")).expect("mkdir");
    write_json(&stale, &fresh);
    set_mtime(&stale, t(21));
}

#[test]
fn a_copy_no_live_session_holds_never_rewrites_home() {
    let home = HomeSandbox::new();
    let home_file = home.home().join(".claude.json");
    let rich = json!({"numStartups": 247, "projects": {"/a": {}, "/b": {}},
        "hasCompletedOnboarding": true, "firstStartTime": "2026-10-28T21:34:15.486Z",
        "userID": "old", "installMethod": "native", "autoUpdates": false});
    write_json(&home_file, &rich);
    set_mtime(&home_file, t(10));
    plant_dead_runtime_copies(home.home());
    reset_last_synced();

    sync_once().expect("sync");

    assert_eq!(
        read_json(&home_file),
        rich,
        "a dead runtime copy took part in the sync and rewrote ~/.claude.json"
    );
}

// ── the reconciler's three safety guards ────────────────────────────────────

/// The backups under `dir` named `<file>.<stamp>`, oldest first. An absent dir
/// has none.
fn backups_named(dir: &Path, file: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let prefix = format!("{file}.");
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    out.sort();
    out
}

/// A path as the refusal line renders it: the sandbox home shortened to `~`.
fn tilde(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Guard 2: a live runtime copy clauth never wrote joins the pass but can never
/// WIN it, so a stray Claude Code's first-start copy cannot delete the shared
/// keys home holds.
#[test]
fn a_live_copy_clauth_never_wrote_never_wins_home() {
    let home = HomeSandbox::new();
    let home_file = home.home().join(".claude.json");
    let rich = json!({"numStartups": 247, "projects": {"/a": {}},
        "userID": "u", "installMethod": "native"});
    write_json(&home_file, &rich);
    set_mtime(&home_file, t(1));
    let stray = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(stray.parent().expect("has parent")).expect("mkdir");
    write_json(
        &stray,
        &json!({"firstStartTime": "2026-10-31T16:24:19.029Z"}),
    );
    set_mtime(&stray, t(20));
    let _live = arm_live_copy(&stray);
    reset_last_synced();

    sync_once().expect("sync");

    assert_eq!(
        read_json(&home_file),
        rich,
        "an unrecorded live copy won the pass and rewrote ~/.claude.json"
    );
}

/// Guard 2's positive control: a copy clauth WROTE, then carrying a newer
/// in-session edit, still propagates to home.
#[test]
fn a_recorded_live_copy_carrying_a_newer_edit_still_reaches_home() {
    let home = HomeSandbox::new();
    let home_file = home.home().join(".claude.json");
    let live = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("p1")
        .join("runtime-4242-0")
        .join(".claude.json");
    fs::create_dir_all(live.parent().expect("has parent")).expect("mkdir");
    write_json(&home_file, &json!({"numStartups": 1, "projects": {}}));
    set_mtime(&home_file, t(1));
    write_recorded_json(
        &written_record_path().expect("record path"),
        &live,
        &json!({"numStartups": 2, "ccLive": true}),
        t(10),
    );
    let _live = arm_live_copy(&live);
    reset_last_synced();

    sync_once().expect("sync");

    assert_eq!(
        read_json(&home_file),
        json!({"numStartups": 2, "ccLive": true}),
        "a recorded live copy's newer edit must still reach home"
    );
}

/// Guard 3 for `.claude.json`: a recorded live winner lacking four of home's
/// shared top-level keys is one over the bound, so the pass writes nothing,
/// backs home up once, and logs the exact line.
#[test]
fn a_winner_lacking_four_shared_keys_writes_nothing_and_backs_home_up() {
    let home = HomeSandbox::new();
    let home_file = home.home().join(".claude.json");
    let winner = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(winner.parent().expect("has parent")).expect("mkdir");
    write_json(
        &home_file,
        &json!({"numStartups": 1, "projects": {}, "userID": "u",
            "installMethod": "native"}),
    );
    set_mtime(&home_file, t(1));
    let record = written_record_path().expect("record path");
    write_recorded_json(&record, &winner, &json!({"firstStartTime": "x"}), t(10));
    let backups_dir = home.home().join(".clauth").join("backups");

    let before_home = fs::read(&home_file).expect("read home");
    let before_winner = fs::read(&winner).expect("read winner");
    let before_record = fs::read(&record).expect("read record");

    let lines = crate::logline::LogLines::new();
    {
        let _capture = lines.capture_here();
        sync_paths(&[home_file.clone(), winner.clone()]).expect("reconcile");
    }

    assert_eq!(
        fs::read(&home_file).expect("read home"),
        before_home,
        "a refused pass must leave home byte-identical"
    );
    assert_eq!(
        fs::read(&winner).expect("read winner"),
        before_winner,
        "a refused pass must leave every member byte-identical"
    );
    assert_eq!(
        fs::read(&record).expect("read record"),
        before_record,
        "a refused pass must not persist a record"
    );
    let backups = backups_named(&backups_dir, ".claude.json");
    assert_eq!(backups.len(), 1, "exactly one backup: {backups:#?}");
    assert_eq!(
        fs::read(&backups[0]).expect("read backup"),
        before_home,
        "the backup holds home's own bytes"
    );
    let expected = format!(
        "clauth: .claude.json sync skipped: applying {} would delete 4 keys from {}; \
         saved a copy at {}",
        tilde(home.home(), &winner),
        tilde(home.home(), &home_file),
        tilde(home.home(), &backups[0]),
    );
    assert_eq!(
        lines.snapshot(),
        vec![expected],
        "the refusal line is pinned exactly"
    );

    {
        let _capture = lines.capture_here();
        sync_paths(&[home_file.clone(), winner.clone()]).expect("second reconcile");
    }
    assert_eq!(
        lines.snapshot().len(),
        1,
        "a second pass over the same winner must not raise a second line"
    );
    assert_eq!(
        backups_named(&backups_dir, ".claude.json").len(),
        1,
        "a second pass over the same winner must not take a second backup"
    );
}

/// The protected-key list belongs to the settings reconciler alone: a
/// `.claude.json` winner that happens to lack a key named `cleanupPeriodDays`
/// still wins, and the pass still deletes that key from home.
#[test]
fn a_cleanup_named_key_is_not_protected_in_the_claude_json_pass() {
    let home = HomeSandbox::new();
    let home_file = home.home().join(".claude.json");
    let live = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("p1")
        .join("runtime-4242-0")
        .join(".claude.json");
    fs::create_dir_all(live.parent().expect("has parent")).expect("mkdir");
    write_json(
        &home_file,
        &json!({"numStartups": 1, "cleanupPeriodDays": 3650}),
    );
    set_mtime(&home_file, t(1));
    write_recorded_json(
        &written_record_path().expect("record path"),
        &live,
        &json!({"numStartups": 2, "ccLive": true}),
        t(10),
    );
    let _live = arm_live_copy(&live);
    reset_last_synced();

    sync_once().expect("sync");

    assert_eq!(
        read_json(&home_file),
        json!({"numStartups": 2, "ccLive": true}),
        "a .claude.json winner lacking cleanupPeriodDays must still run and delete it"
    );
}

// ── the guards pinned one at a time ──────────────────────────────────────────

/// Guard 1 alone for `.claude.json`: a DEAD recorded copy (so guard 2 lets it
/// win), newest, and lacking only one shared key (so guard 3 lets the pass run)
/// is still excluded by membership, so home keeps its keys.
#[test]
fn a_dead_recorded_copy_cannot_wipe_home_when_only_membership_can_stop_it() {
    let home = HomeSandbox::new();
    let home_file = home.home().join(".claude.json");
    let rich = json!({"numStartups": 247, "projects": {"/a": {}},
        "userID": "u", "installMethod": "native"});
    write_json(&home_file, &rich);
    set_mtime(&home_file, t(10));
    // A dead tree under a profile clauth never made, and one beside a sessions
    // dir no process holds: both recorded and newest, each lacking only one
    // shared key.
    let ghost = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("zcode9")
        .join("runtime-3532172-11")
        .join(".claude.json");
    write_recorded_json(
        &written_record_path().expect("record path"),
        &ghost,
        &json!({"numStartups": 247, "projects": {"/a": {}}, "userID": "u"}),
        t(20),
    );
    let stale = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("cld0")
        .join("runtime-4242-0")
        .join(".claude.json");
    fs::create_dir_all(
        home.home()
            .join(".clauth")
            .join("profiles")
            .join("cld0")
            .join("sessions-4242-0"),
    )
    .expect("mkdir");
    write_recorded_json(
        &written_record_path().expect("record path"),
        &stale,
        &json!({"numStartups": 247, "projects": {"/a": {}}, "userID": "u"}),
        t(21),
    );
    reset_last_synced();

    sync_once().expect("sync");

    assert_eq!(
        read_json(&home_file),
        rich,
        "a dead recorded copy took part in the sync and rewrote ~/.claude.json"
    );
}

/// A live session's seed that no record names (a seed that missed the sync
/// lock, stamped at the floor) but whose bytes already equal the merge is
/// recorded on the first pass that sees it, without a rewrite, so the
/// session's later Claude Code edit wins and reaches home.
#[test]
fn an_unrecorded_live_seed_matching_the_merge_is_recorded_and_its_edit_reaches_home() {
    let home = HomeSandbox::new();
    let home_file = home.home().join(".claude.json");
    write_json(&home_file, &json!({"numStartups": 2, "userID": "u"}));
    set_mtime(&home_file, t(1));
    let seed = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(seed.parent().expect("has parent")).expect("mkdir");
    write_json(
        &seed,
        &json!({"numStartups": 2, "userID": "u", "oauthAccount": {"emailAddress": "p1@x"}}),
    );
    set_mtime(&seed, crate::jsonsync::floor_stamp());
    let _live = arm_live_copy(&seed);
    crate::jsonsync::reset_write_counts();
    reset_last_synced();

    sync_once().expect("sync");

    assert_eq!(
        crate::jsonsync::write_count(&seed),
        0,
        "a seed already equal to the merge is recorded without a rewrite"
    );
    let entries: Vec<Value> = fs::read(written_record_path().expect("record path"))
        .map(|bytes| serde_json::from_slice(&bytes).expect("record parses"))
        .unwrap_or_default();
    // Compared as paths, never strings: the seed path is joined with `/` while
    // the record holds the platform's own separator.
    assert!(
        entries
            .iter()
            .any(|e| e["path"].as_str().map(Path::new) == Some(seed.as_path())),
        "the matching live seed must be recorded on first sight: {entries:#?}"
    );

    write_json(
        &seed,
        &json!({"numStartups": 5, "userID": "u", "oauthAccount": {"emailAddress": "p1@x"}}),
    );
    set_mtime(&seed, t(20));
    reset_last_synced();
    sync_once().expect("sync");

    assert_eq!(
        read_json(&home_file),
        json!({"numStartups": 5, "userID": "u"}),
        "the session's edit must reach home once its seed is recorded"
    );
}

/// Guard 2 alone for `.claude.json`: a LIVE unrecorded copy, newest and lacking
/// only one shared key, is admitted (guard 1) and under the bound (guard 3) —
/// only the winner gate keeps home intact.
#[test]
fn an_unrecorded_live_copy_cannot_win_when_only_the_winner_gate_can_stop_it() {
    let home = HomeSandbox::new();
    let home_file = home.home().join(".claude.json");
    let rich = json!({"numStartups": 247, "projects": {"/a": {}},
        "userID": "u", "installMethod": "native"});
    write_json(&home_file, &rich);
    set_mtime(&home_file, t(1));
    let stray = home
        .home()
        .join(".clauth/profiles/p1/runtime-4242-0/.claude.json");
    fs::create_dir_all(stray.parent().expect("has parent")).expect("mkdir");
    write_json(
        &stray,
        &json!({"numStartups": 247, "projects": {"/a": {}}, "userID": "u"}),
    );
    set_mtime(&stray, t(20));
    let _live = arm_live_copy(&stray);
    reset_last_synced();

    sync_once().expect("sync");

    assert_eq!(
        read_json(&home_file),
        rich,
        "an unrecorded live copy won the pass and rewrote ~/.claude.json"
    );
}
