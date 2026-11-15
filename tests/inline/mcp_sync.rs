#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::profile::ProfileName;
use crate::testutil::HomeSandbox;
use std::fs;

/// One MCP login: `access`, `refresh` and its expiry.
fn login(access: &str, refresh: &str, expires: i64) -> Value {
    serde_json::json!({ "accessToken": access, "refreshToken": refresh, "expiresAt": expires })
}

/// A store holding a Claude login and `mcp` as its `mcpOAuth`.
fn store_value(mcp: Value) -> Value {
    serde_json::json!({ "claudeAiOauth": { "accessToken": "login" }, "mcpOAuth": mcp })
}

fn store(name: &str) -> PathBuf {
    crate::profile::profile_dir(&ProfileName::from(name))
        .expect("profile dir")
        .join("credentials.json")
}

/// Accounts `accounts`, each with a `credentials.json` holding its `mcpOAuth`.
fn seed(accounts: &[(&str, Value)]) {
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: accounts.iter().map(|(n, _)| (*n).into()).collect(),
        ..crate::profile::AppState::default()
    })
    .expect("save state");
    for (name, mcp) in accounts {
        crate::profile::save_profile(&crate::profile::Profile::new(
            (*name).to_string(),
            None,
            None,
        ))
        .expect("save profile");
        write_mcp(name, mcp.clone());
    }
}

fn write_mcp(name: &str, mcp: Value) {
    fs::write(store(name), store_value(mcp).to_string()).expect("store");
}

fn mcp_of(name: &str) -> Value {
    let value: Value = serde_json::from_slice(&fs::read(store(name)).expect("read")).expect("json");
    value["mcpOAuth"].clone()
}

/// One fan-out-shaped pass over `names`' stores, read as sources.
fn pass(names: &[&str]) -> Vec<(PathBuf, Vec<String>)> {
    let paths: Vec<PathBuf> = names.iter().map(|n| store(n)).collect();
    crate::claude::sync_mcp_into_stores(&paths, &[])
}

fn source(key: &str, mcp: Value) -> Source {
    Source {
        key: Some(key.to_string()),
        value: Some(store_value(mcp)),
    }
}

// ── a usable entry is replaced only by a copy of the same login ────────────

/// Two accounts that each logged in to `srv` on their own keep their own
/// logins, whichever expires later; an account with no copy takes the newest.
#[test]
fn a_usable_independent_login_is_never_replaced_by_another_accounts_fresher_copy() {
    let _home = HomeSandbox::new();
    let a = login("a", "ra", 100);
    let b = login("b", "rb", 500);
    seed(&[
        ("alpha", serde_json::json!({ "srv": a })),
        ("beta", serde_json::json!({ "srv": b })),
        ("gamma", serde_json::json!({})),
    ]);

    pass(&["alpha", "beta", "gamma"]);

    assert_eq!(mcp_of("alpha")["srv"], a, "alpha keeps its own login");
    assert_eq!(mcp_of("beta")["srv"], b);
    assert_eq!(mcp_of("gamma")["srv"], b, "a gap takes the newest copy");
}

/// An absent entry and a signed-out one (neither token) are gaps: each takes
/// the newest usable copy of any lineage.
#[test]
fn a_gap_takes_the_newest_copy_of_any_lineage() {
    let _home = HomeSandbox::new();
    let older = login("old", "r-old", 100);
    let newer = login("new", "r-new", 900);
    seed(&[
        ("alpha", serde_json::json!({ "srv": older })),
        ("beta", serde_json::json!({ "srv": newer })),
        ("gap", serde_json::json!({})),
        (
            "signed",
            serde_json::json!({ "srv": { "serverName": "srv" } }),
        ),
    ]);

    pass(&["alpha", "beta", "gap", "signed"]);

    assert_eq!(mcp_of("gap")["srv"], newer);
    assert_eq!(mcp_of("signed")["srv"], newer);
}

/// The trap: after one source rotates its copy, every other copy holds the
/// spent token, which matches no source's token now. The rotation is an
/// in-place change of a source the record saw, so it stays the same login and
/// every copy takes the rotated one.
#[test]
fn a_rotation_in_one_source_replaces_every_copy_of_that_lineage() {
    let _home = HomeSandbox::new();
    seed(&[
        ("one", serde_json::json!({ "srv": login("a1", "r1", 100) })),
        ("two", serde_json::json!({})),
        ("three", serde_json::json!({})),
    ]);
    pass(&["one", "two", "three"]);
    assert_eq!(mcp_of("two")["srv"]["refreshToken"], "r1");

    let rotated = login("a2", "r2", 200);
    write_mcp("one", serde_json::json!({ "srv": rotated }));
    pass(&["one", "two", "three"]);

    assert_eq!(mcp_of("two")["srv"], rotated);
    assert_eq!(mcp_of("three")["srv"], rotated);
}

/// A gap the sync fills is recorded as a copy of the winner's lineage, so
/// the target's earlier lineage gains nothing from the fill: a later rotation
/// of the winner reaches the filled store.
#[test]
fn the_syncs_own_write_is_no_rotation_of_the_targets_old_lineage() {
    let _home = HomeSandbox::new();
    seed(&[
        (
            "signed",
            serde_json::json!({ "srv": login("x", "rx", 100) }),
        ),
        ("owner", serde_json::json!({ "srv": login("w", "rw", 300) })),
    ]);
    pass(&["signed", "owner"]);
    let x_lineage = SyncState::load()
        .lineage_holding("srv", &fingerprint(&login("x", "rx", 100)).unwrap())
        .expect("x's lineage");
    write_mcp(
        "signed",
        serde_json::json!({ "srv": { "serverName": "srv" } }),
    );
    pass(&["signed", "owner"]);
    assert_eq!(
        mcp_of("signed")["srv"]["refreshToken"],
        "rw",
        "the gap is filled"
    );

    let w_fp = fingerprint(&login("w", "rw", 300)).unwrap();
    let state = SyncState::load();
    assert!(
        !state.lineages[&x_lineage].fps.contains(&w_fp),
        "the fill folds nothing into the old lineage"
    );
    let rotated = login("w2", "rw2", 600);
    write_mcp("owner", serde_json::json!({ "srv": rotated }));
    pass(&["signed", "owner"]);
    assert_eq!(mcp_of("signed")["srv"], rotated);
}

/// A source whose new fingerprint another lineage already holds moved onto
/// that login (a switch carry, a hand-off): it joins that lineage and its old
/// lineage gains nothing.
#[test]
fn a_carry_onto_a_known_fp_moves_the_source_and_merges_nothing() {
    let a = login("a", "ra", 100);
    let b = login("b", "rb", 200);
    let mut state = SyncState::default();
    state.observe(
        &[
            source("path:q", serde_json::json!({ "srv": a })),
            source("path:other", serde_json::json!({ "srv": b })),
        ],
        1,
        true,
    );
    let a_id = state
        .lineage_holding("srv", &fingerprint(&a).unwrap())
        .unwrap();
    let b_id = state
        .lineage_holding("srv", &fingerprint(&b).unwrap())
        .unwrap();

    state.observe(
        &[
            source("path:q", serde_json::json!({ "srv": b })),
            source("path:other", serde_json::json!({ "srv": b })),
        ],
        2,
        false,
    );

    assert_eq!(
        state.seen["path:q"]["srv"].lineage.as_deref(),
        Some(b_id.as_str())
    );
    assert_eq!(
        state.lineages[&a_id].fps,
        BTreeSet::from([fingerprint(&a).unwrap()])
    );
}

/// With the record lost every usable entry is a first sight: a copy holding
/// a rotated-away token is kept, never overwritten. A torn record reads the
/// same way, is moved aside owner-only, and a fresh owner-only record lands.
#[test]
fn a_lost_or_torn_record_fills_gaps_only() {
    let _home = HomeSandbox::new();
    seed(&[
        ("one", serde_json::json!({ "srv": login("a1", "r1", 100) })),
        ("two", serde_json::json!({})),
    ]);
    pass(&["one", "two"]);
    let copy = mcp_of("two")["srv"].clone();
    assert_eq!(copy["refreshToken"], "r1");
    let record = state_path().expect("record path");
    fs::remove_file(&record).expect("lose the record");
    write_mcp("one", serde_json::json!({ "srv": login("a2", "r2", 200) }));

    pass(&["one", "two"]);
    assert_eq!(mcp_of("two")["srv"], copy, "a lost record never overwrites");

    fs::write(&record, "{ torn").expect("tear the record");
    write_mcp("one", serde_json::json!({ "srv": login("a3", "r3", 300) }));
    pass(&["one", "two"]);
    assert_eq!(mcp_of("two")["srv"], copy, "a torn record never overwrites");
    let dir = record.parent().expect("clauth dir");
    let aside: Vec<PathBuf> = fs::read_dir(dir)
        .expect("list")
        .map(|e| e.expect("entry").path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("mcp-sync-state.json.corrupt-"))
        })
        .collect();
    assert_eq!(aside.len(), 1, "the torn record is moved aside: {aside:?}");
    assert_eq!(fs::read_to_string(&aside[0]).expect("read aside"), "{ torn");
    assert!(
        matches!(read_disk(&record), Disk::Read(_)),
        "a fresh record landed"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&record, &aside[0]] {
            let mode = fs::metadata(path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
    }
}

/// The record holds digests and the servers' keys, never a token, and is
/// owner-only.
#[test]
fn the_record_holds_no_token_and_is_owner_only() {
    let _home = HomeSandbox::new();
    seed(&[
        (
            "alpha",
            serde_json::json!({ "srv": login("access-secret", "refresh-secret", 100) }),
        ),
        ("beta", serde_json::json!({})),
    ]);
    pass(&["alpha", "beta"]);

    let record = state_path().expect("record path");
    let text = fs::read_to_string(&record).expect("read record");
    assert!(text.contains("\"srv\""), "the record names the server");
    for secret in ["access-secret", "refresh-secret"] {
        assert!(!text.contains(secret), "the record holds no token");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&record).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

/// A write-back keeps what a concurrent writer saved since this pass loaded
/// the record: its observation of another source and the fingerprint it
/// added to a lineage.
#[test]
fn a_record_write_back_keeps_a_concurrent_fan_outs_observation() {
    let _home = HomeSandbox::new();
    let a = login("a", "ra", 100);
    let mut loaded = SyncState::default();
    loaded.observe(
        &[source("path:s", serde_json::json!({ "srv": a }))],
        1,
        true,
    );
    let a_id = loaded
        .lineage_holding("srv", &fingerprint(&a).unwrap())
        .unwrap();
    crate::lock::with_state_lock(|_held| {
        crate::profile::atomic_write_600(&state_path()?, serde_json::to_vec(&loaded)?)
            .map_err(anyhow::Error::from)
    })
    .expect("seed the record");

    let mut theirs = loaded.clone();
    let rotated = login("a2", "ra2", 200);
    theirs.observe(
        &[
            source("path:s", serde_json::json!({ "srv": rotated })),
            source("item:/t/runtime-1", serde_json::json!({ "srv": rotated })),
        ],
        2,
        false,
    );
    crate::lock::with_state_lock(|_held| {
        crate::profile::atomic_write_600(&state_path()?, serde_json::to_vec(&theirs)?)
            .map_err(anyhow::Error::from)
    })
    .expect("a concurrent writer saves");

    let mut ours = loaded.clone();
    let rewritten = ours.observe(
        &[source("path:other", serde_json::json!({ "srv": a }))],
        3,
        false,
    );
    write_back(&loaded, &ours, &rewritten).expect("write back");

    let on_disk = SyncState::load();
    assert!(
        on_disk.seen.contains_key("item:/t/runtime-1"),
        "their source stays"
    );
    assert!(on_disk.seen.contains_key("path:other"), "ours lands");
    assert!(
        on_disk.lineages[&a_id]
            .fps
            .contains(&fingerprint(&rotated).unwrap()),
        "their fingerprint stays"
    );
}

// ── a vanished entry is a removal, never a gap ─────────────────────────────

/// A sign-out in one store removes that login from every store the sync may
/// write, and nothing re-fills it.
#[test]
fn a_sign_out_in_one_store_removes_that_login_from_every_writable_store() {
    let _home = HomeSandbox::new();
    let l = login("l", "rl", 100);
    let names = ["one", "two", "three"];
    seed(&[
        (
            "one",
            serde_json::json!({ "srv": l, "keep": login("k", "rk", 5) }),
        ),
        ("two", serde_json::json!({ "srv": l })),
        ("three", serde_json::json!({ "srv": l })),
    ]);
    pass(&names);
    write_mcp("one", serde_json::json!({ "keep": login("k", "rk", 5) }));

    pass(&names);
    for name in names {
        assert!(
            mcp_of(name)["srv"].is_null(),
            "{name} lost the signed-out login"
        );
    }
    pass(&names);
    for name in names {
        assert!(mcp_of(name)["srv"].is_null(), "{name} is not re-filled");
        assert_eq!(
            mcp_of(name)["keep"]["refreshToken"],
            "rk",
            "{name} keeps the other server"
        );
    }
}

/// The removal is scoped to the signed-out login: another account's own
/// login of the same server stays, and does not fill the stores that signed
/// out.
#[test]
fn a_sign_out_never_removes_an_independent_login_of_the_same_server() {
    let _home = HomeSandbox::new();
    let l = login("l", "rl", 100);
    let m = login("m", "rm", 900);
    let names = ["one", "two", "own"];
    seed(&[
        ("one", serde_json::json!({ "srv": l })),
        ("two", serde_json::json!({ "srv": l })),
        ("own", serde_json::json!({ "srv": m })),
    ]);
    pass(&names);
    write_mcp("one", serde_json::json!({}));

    pass(&names);
    pass(&names);

    assert_eq!(mcp_of("own")["srv"], m, "the independent login stays");
    assert!(
        mcp_of("one")["srv"].is_null(),
        "one signed out stays signed out"
    );
    assert!(
        mcp_of("two")["srv"].is_null(),
        "two lost the signed-out login"
    );
}

/// A copy of a signed-out login in a source the sync never writes (a live
/// session's own file) that changes in place is a new login, a refresh as
/// much as a re-login (the two look alike): it reaches the store that signed
/// out.
#[test]
fn a_tombstoned_copy_changing_in_place_is_a_new_login() {
    let l = login("l", "rl", 100);
    let mut state = SyncState::default();
    let all =
        |store: Value, live: Value| vec![source("path:store", store), source("path:live", live)];
    state.observe(
        &all(
            serde_json::json!({ "srv": l }),
            serde_json::json!({ "srv": l }),
        ),
        1,
        true,
    );
    state.observe(
        &all(serde_json::json!({}), serde_json::json!({ "srv": l })),
        2,
        true,
    );
    let rotated = login("l2", "rl2", 200);
    let sources = all(serde_json::json!({}), serde_json::json!({ "srv": rotated }));
    state.observe(&sources, 3, true);

    let plan = state.plan(&sources);
    let mut target = store_value(serde_json::json!({}));
    assert_eq!(state.apply(Some("path:store"), &mut target, &plan), ["srv"]);
    assert_eq!(target["mcpOAuth"]["srv"], rotated);
}

/// A login made again inside a running session whose linked store still holds
/// a copy of a login signed out elsewhere reaches every store that signed out
/// or lost the copy, and survives that session's end.
#[cfg(unix)]
#[test]
fn a_re_login_inside_a_session_holding_a_signed_out_copy_spreads_and_survives() {
    let _home = HomeSandbox::new();
    let l = login("l", "rl", 100);
    let names = ["linked", "spare", "late"];
    seed(&[
        ("linked", serde_json::json!({ "srv": l })),
        ("spare", serde_json::json!({ "srv": l })),
        ("late", serde_json::json!({ "srv": l })),
    ]);
    let sid = "6262-0";
    crate::live_sessions::register(&crate::testutil::live_row(sid, "linked")).expect("register");
    let marker = crate::runtime::hold_session_row_marker(&ProfileName::from("linked"), false, sid)
        .expect("hold the marker");
    let [tree, _] = crate::runtime::session_runtime_trees(&ProfileName::from("linked"), false, sid)
        .expect("trees");
    fs::create_dir_all(&tree).expect("tree");
    std::os::unix::fs::symlink(store("linked"), tree.join(".credentials.json")).expect("link");
    pass(&names);
    write_mcp("spare", serde_json::json!({}));
    pass(&names);
    assert!(
        mcp_of("late")["srv"].is_null(),
        "late lost the signed-out login"
    );

    let again = login("n", "rn", 500);
    write_mcp("linked", serde_json::json!({ "srv": again }));
    pass(&names);
    for name in names {
        assert_eq!(mcp_of(name)["srv"], again, "{name} holds the new login");
    }

    drop(marker);
    crate::live_sessions::unregister(sid).expect("unregister");
    pass(&names);
    pass(&names);
    for name in names {
        assert_eq!(
            mcp_of(name)["srv"],
            again,
            "{name} keeps it after the session"
        );
    }
}

/// A copy changed in place in the very pass that sees another source sign out
/// of its login is a new login too: the sign-out is read first.
#[test]
fn a_change_in_place_in_the_sign_out_pass_is_a_new_login() {
    let l = login("l", "rl", 100);
    let again = login("n", "rn", 500);
    let mut state = SyncState::default();
    state.observe(
        &[
            source("path:a", serde_json::json!({ "srv": l })),
            source("path:b", serde_json::json!({ "srv": l })),
        ],
        1,
        true,
    );
    let sources = [
        source("path:a", serde_json::json!({})),
        source("path:b", serde_json::json!({ "srv": again })),
    ];
    state.observe(&sources, 2, true);

    let plan = state.plan(&sources);
    let mut a = store_value(serde_json::json!({}));
    assert_eq!(state.apply(Some("path:a"), &mut a, &plan), ["srv"]);
    assert_eq!(a["mcpOAuth"]["srv"], again);
}

/// A store a running session reads through its link keeps a signed-out
/// login while that session runs, and loses it once the session ends.
#[cfg(unix)]
#[test]
fn a_linked_store_loses_a_tombstoned_login_once_its_session_ends() {
    let _home = HomeSandbox::new();
    let l = login("l", "rl", 100);
    let names = ["linked", "spare"];
    seed(&[
        ("linked", serde_json::json!({ "srv": l })),
        ("spare", serde_json::json!({ "srv": l })),
    ]);
    let sid = "6161-0";
    crate::live_sessions::register(&crate::testutil::live_row(sid, "linked")).expect("register");
    let marker = crate::runtime::hold_session_row_marker(&ProfileName::from("linked"), false, sid)
        .expect("hold the marker");
    let [tree, _] = crate::runtime::session_runtime_trees(&ProfileName::from("linked"), false, sid)
        .expect("trees");
    fs::create_dir_all(&tree).expect("tree");
    std::os::unix::fs::symlink(store("linked"), tree.join(".credentials.json")).expect("link");
    pass(&names);
    write_mcp("spare", serde_json::json!({}));

    pass(&names);
    assert_eq!(
        mcp_of("linked")["srv"],
        l,
        "a store a running session links is left"
    );

    drop(marker);
    crate::live_sessions::unregister(sid).expect("unregister");
    pass(&names);
    assert!(
        mcp_of("linked")["srv"].is_null(),
        "once the session ends it is removed"
    );
}

/// A parked copy of a signed-out login is never written, and the store it is
/// restored into loses it at the next pass.
#[test]
fn a_parked_copy_of_a_tombstoned_login_is_removed_after_its_restore() {
    let l = login("l", "rl", 100);
    let mut state = SyncState::default();
    let parked = source("path:parked", serde_json::json!({ "srv": l }));
    state.observe(
        &[
            parked.clone(),
            source("path:store", serde_json::json!({ "srv": l })),
        ],
        1,
        true,
    );
    let sources = [parked.clone(), source("path:store", serde_json::json!({}))];
    state.observe(&sources, 2, true);
    let plan = state.plan(&sources);
    let mut empty = store_value(serde_json::json!({}));
    assert!(
        state
            .apply(Some("path:store"), &mut empty, &plan)
            .is_empty(),
        "the parked copy is no source while signed out"
    );

    let restored = [
        parked,
        source("path:store", serde_json::json!({ "srv": l })),
    ];
    state.observe(&restored, 3, true);
    let plan = state.plan(&restored);
    let mut target = store_value(serde_json::json!({ "srv": l }));
    assert_eq!(state.apply(Some("path:store"), &mut target, &plan), ["srv"]);
    assert!(
        target["mcpOAuth"]["srv"].is_null(),
        "the restored copy is removed"
    );
}

/// A login made after a sign-out is a new login: it reaches every store,
/// those that signed out included.
#[test]
fn a_new_login_after_a_sign_out_propagates() {
    let _home = HomeSandbox::new();
    let names = ["one", "two"];
    seed(&[
        ("one", serde_json::json!({ "srv": login("l", "rl", 100) })),
        ("two", serde_json::json!({ "srv": login("l", "rl", 100) })),
    ]);
    pass(&names);
    write_mcp("one", serde_json::json!({}));
    pass(&names);
    assert!(mcp_of("two")["srv"].is_null());

    let fresh = login("n", "rn", 500);
    write_mcp("one", serde_json::json!({ "srv": fresh }));
    pass(&names);

    assert_eq!(mcp_of("two")["srv"], fresh);
}

/// A login first seen in the very pass that sees a sign-out, from a source
/// that never held the signed-out login, is a new login: it reaches the store
/// that signed out and every store whose copy that pass removes. Another
/// account's own login the record knew before the sign-out reaches none of
/// them, and a copy of the signed-out login turning up in that pass never
/// comes back, though it would outlast the new one.
#[test]
fn a_new_login_first_seen_in_the_sign_out_pass_reaches_the_stores_that_signed_out() {
    let _home = HomeSandbox::new();
    let l = login("l", "rl", 700);
    let m = login("m", "rm", 900);
    let x = login("x", "rx", 500);
    seed(&[
        ("one", serde_json::json!({ "srv": l })),
        ("two", serde_json::json!({ "srv": l })),
        ("own", serde_json::json!({ "srv": m })),
        ("four", serde_json::json!({ "srv": x })),
        ("five", serde_json::json!({ "srv": l })),
    ]);
    pass(&["one", "two", "own"]);
    write_mcp("one", serde_json::json!({}));

    let all = ["one", "two", "own", "four", "five"];
    for round in 1..=2 {
        pass(&all);
        for name in ["one", "two", "four", "five"] {
            assert_eq!(
                mcp_of(name)["srv"],
                x,
                "pass {round}: {name} holds the new login"
            );
        }
        assert_eq!(mcp_of("own")["srv"], m, "pass {round}: own keeps its own");
    }
}

/// A store that signed out of a server and later holds a signed-out entry for
/// it (neither token) has not logged in again: it keeps refusing another
/// account's own login, as the store without the entry does.
#[test]
fn a_signed_out_entry_keeps_a_stores_sign_out() {
    let _home = HomeSandbox::new();
    let l = login("l", "rl", 100);
    let m = login("m", "rm", 900);
    let names = ["one", "two", "own"];
    seed(&[
        ("one", serde_json::json!({ "srv": l })),
        ("two", serde_json::json!({ "srv": l })),
        ("own", serde_json::json!({ "srv": m })),
    ]);
    pass(&names);
    write_mcp("one", serde_json::json!({}));
    pass(&names);
    let signed_out = serde_json::json!({ "serverName": "srv" });
    write_mcp("one", serde_json::json!({ "srv": signed_out }));

    pass(&names);
    pass(&names);

    assert_eq!(
        mcp_of("one")["srv"],
        signed_out,
        "one keeps its signed-out entry"
    );
    assert!(mcp_of("two")["srv"].is_null());
    assert_eq!(mcp_of("own")["srv"], m);
}

/// Another account's own login the record forgot (no source held it) and a
/// copy of it put back later mints the lineage id it had: it reads as the
/// new login it now is, so a store that refused it at a sign-out takes it.
#[test]
fn a_forgotten_rival_put_back_is_no_longer_refused() {
    let l = login("l", "rl", 100);
    let m = login("m", "rm", 900);
    let mut state = SyncState::default();
    state.observe(
        &[
            source("path:a", serde_json::json!({ "srv": l })),
            source("path:b", serde_json::json!({ "srv": m })),
        ],
        1,
        true,
    );
    state.observe(
        &[
            source("path:a", serde_json::json!({})),
            source("path:b", serde_json::json!({ "srv": m })),
        ],
        2,
        true,
    );
    state.observe(&[source("path:a", serde_json::json!({}))], 3, true);
    assert!(
        state
            .lineage_holding("srv", &fingerprint(&m).unwrap())
            .is_none(),
        "no source held own's login, so the record forgot it"
    );

    let restored = [
        source("path:a", serde_json::json!({})),
        source("path:b", serde_json::json!({ "srv": m })),
    ];
    state.observe(&restored, 4, true);
    let plan = state.plan(&restored);
    let mut a = store_value(serde_json::json!({}));
    assert_eq!(state.apply(Some("path:a"), &mut a, &plan), ["srv"]);
    assert_eq!(a["mcpOAuth"]["srv"], m, "the put-back login reaches a");
}

/// A store whose file lost its whole `mcpOAuth` object (rebuilt by a fresh
/// login) signed out of nothing: every other store keeps the login, and the
/// rebuilt one takes the newest copies again.
#[test]
fn a_rebuilt_store_with_no_mcp_logins_is_refilled_never_a_sign_out() {
    let _home = HomeSandbox::new();
    let l = login("l", "rl", 100);
    let names = ["one", "two"];
    seed(&[
        ("one", serde_json::json!({ "srv": l })),
        ("two", serde_json::json!({ "srv": l })),
    ]);
    pass(&names);
    fs::write(
        store("one"),
        serde_json::json!({ "claudeAiOauth": { "accessToken": "relogin" } }).to_string(),
    )
    .expect("rebuilt store");

    pass(&names);
    pass(&names);

    assert_eq!(mcp_of("two")["srv"], l, "two keeps the login");
    assert_eq!(mcp_of("one")["srv"], l, "the rebuilt store is refilled");
}

/// A rebuilt store's record is cleared: a login saved there later (here
/// while the sync cannot write the store) is a first sight, never folded
/// into the login it held before as an in-place rotation, so the stores still
/// holding that earlier login keep it.
#[test]
fn a_login_saved_into_a_rebuilt_store_is_a_new_login() {
    let x1 = login("x1", "r1", 100);
    let x2 = login("x2", "r2", 900);
    let mut state = SyncState::default();
    let rebuilt = Source {
        key: Some("path:s".to_string()),
        value: Some(serde_json::json!({ "claudeAiOauth": { "accessToken": "relogin" } })),
    };
    state.observe(
        &[
            source("path:s", serde_json::json!({ "srv": x1 })),
            source("path:t", serde_json::json!({ "srv": x1 })),
        ],
        1,
        true,
    );
    state.observe(
        &[rebuilt, source("path:t", serde_json::json!({ "srv": x1 }))],
        2,
        true,
    );
    let sources = [
        source("path:s", serde_json::json!({ "srv": x2 })),
        source("path:t", serde_json::json!({ "srv": x1 })),
    ];
    state.observe(&sources, 3, true);

    assert_ne!(
        state.lineage_holding("srv", &fingerprint(&x2).unwrap()),
        state.lineage_holding("srv", &fingerprint(&x1).unwrap()),
        "the later login is a lineage of its own"
    );
    let plan = state.plan(&sources);
    let mut t = store_value(serde_json::json!({ "srv": x1 }));
    assert!(state.apply(Some("path:t"), &mut t, &plan).is_empty());
    assert_eq!(t["mcpOAuth"]["srv"], x1, "t keeps its login");
}

/// A live link and the store it names are one source read twice in a pass:
/// the second read never wipes what the first recorded, so a sign-out made
/// through the link is still seen.
#[test]
fn a_store_read_twice_through_its_link_still_signs_out() {
    let l = login("l", "rl", 100);
    let mut state = SyncState::default();
    let both = |mcp: Value| vec![source("path:store", mcp.clone()), source("path:store", mcp)];
    let mut sources = both(serde_json::json!({ "srv": l }));
    sources.push(source("path:other", serde_json::json!({ "srv": l })));
    state.observe(&sources, 1, true);
    assert!(
        state.seen.contains_key("path:store"),
        "the store is recorded"
    );

    let mut sources = both(serde_json::json!({}));
    sources.push(source("path:other", serde_json::json!({ "srv": l })));
    state.observe(&sources, 2, true);

    let id = state
        .lineage_holding("srv", &fingerprint(&l).unwrap())
        .unwrap();
    assert!(
        state.tombstoned(&id),
        "the sign-out through the link is seen"
    );
}

/// A source that did not read this pass neither signs out of what it held nor
/// lets a tombstone end.
#[test]
fn an_unreadable_source_neither_vanishes_nor_clears_a_tombstone() {
    let l = login("l", "rl", 100);
    let mut state = SyncState::default();
    state.observe(
        &[
            source("path:a", serde_json::json!({ "srv": l })),
            source("path:b", serde_json::json!({ "srv": l })),
        ],
        1,
        true,
    );
    let unread = Source {
        key: Some("path:b".to_string()),
        value: None,
    };
    state.observe(
        &[
            source("path:a", serde_json::json!({ "srv": l })),
            unread.clone(),
        ],
        2,
        true,
    );
    let id = state
        .lineage_holding("srv", &fingerprint(&l).unwrap())
        .unwrap();
    assert!(
        !state.tombstoned(&id),
        "an unread source signs out of nothing"
    );

    state.observe(
        &[source("path:a", serde_json::json!({})), unread.clone()],
        3,
        true,
    );
    assert!(state.tombstoned(&id));
    // Every holder and mark gone with their sources, and a source the record
    // never saw does not read: it may hold a copy, so the tombstone stays.
    let never_seen = Source {
        key: Some("path:new".to_string()),
        value: None,
    };
    state.observe(&[never_seen], 4, true);
    assert!(state.tombstoned(&id), "kept while a source did not read");
    state.observe(&[], 5, true);
    assert!(
        !state.lineages.contains_key(&id),
        "dropped after a pass that read every source"
    );
}

/// A sign-out made while no pass ran (the daemon down) is still a removal:
/// the record the earlier pass saved is what the next one compares against,
/// whichever process runs it.
#[test]
fn a_sign_out_while_the_daemon_was_down_is_still_a_removal() {
    let _home = HomeSandbox::new();
    let l = login("l", "rl", 100);
    seed(&[
        ("one", serde_json::json!({ "srv": l })),
        ("two", serde_json::json!({ "srv": l })),
    ]);
    crate::runtime::sync_mcp_logins_once();
    assert!(
        state_path().expect("path").is_file(),
        "the daemon's pass saved its record"
    );
    write_mcp("one", serde_json::json!({}));

    crate::runtime::sync_mcp_logins_once();

    assert!(mcp_of("two")["srv"].is_null());
    assert!(mcp_of("one")["srv"].is_null());
}

/// A tombstone lasts while a source still holds a copy of the signed-out
/// login, and ends at the first daemon pass that read every source and found
/// none: the record forgets that login. Each store that signed out keeps its
/// removal mark, so another account's own login, there to take before the
/// sign-out, still never fills it.
#[test]
fn a_tombstone_ends_once_no_source_holds_a_copy() {
    let _home = HomeSandbox::new();
    let l = login("l", "rl", 100);
    let m = login("m", "rm", 900);
    seed(&[
        ("one", serde_json::json!({ "srv": l })),
        ("two", serde_json::json!({ "srv": l })),
        ("own", serde_json::json!({ "srv": m })),
    ]);
    let l_fp = fingerprint(&l).unwrap();
    let m_fp = fingerprint(&m).unwrap();
    crate::runtime::sync_mcp_logins_once();
    write_mcp("one", serde_json::json!({}));

    crate::runtime::sync_mcp_logins_once();
    let id = SyncState::load()
        .lineage_holding("srv", &l_fp)
        .expect("the signed-out login is still known");
    assert!(
        SyncState::load().tombstoned(&id),
        "two held a copy at that pass, so the tombstone stands"
    );
    assert!(mcp_of("two")["srv"].is_null(), "two's copy was removed");

    for round in 3..=4 {
        crate::runtime::sync_mcp_logins_once();
        let state = SyncState::load();
        let fps: Vec<&String> = state.lineages.values().flat_map(|l| &l.fps).collect();
        assert_eq!(fps, [&m_fp], "pass {round}: only own's login is known");
        for name in ["one", "two"] {
            let mark = &state.seen[&path_key(&store(name))]["srv"];
            assert!(mark.removed, "pass {round}: {name} keeps its removal mark");
            assert!(
                mcp_of(name)["srv"].is_null(),
                "pass {round}: {name} stays out"
            );
        }
        assert_eq!(mcp_of("own")["srv"], m);
    }
}

/// A pass that ended a tombstone writes back over a concurrent writer that
/// saw a copy of that login change in place meanwhile: the change is the new
/// login it reads as on either side, and the signed-out lineage stays ended.
#[test]
fn a_concurrent_writers_change_in_place_stays_a_new_login_over_an_ended_tombstone() {
    let l = login("l", "rl", 100);
    let mut loaded = SyncState::default();
    loaded.observe(
        &[
            source("path:a", serde_json::json!({ "srv": l })),
            source("path:b", serde_json::json!({ "srv": l })),
        ],
        1,
        true,
    );
    loaded.observe(
        &[
            source("path:a", serde_json::json!({})),
            source("path:b", serde_json::json!({ "srv": l })),
        ],
        2,
        true,
    );
    let id = loaded
        .lineage_holding("srv", &fingerprint(&l).unwrap())
        .unwrap();
    assert!(loaded.tombstoned(&id));

    let mut ours = loaded.clone();
    let rewritten = ours.observe(
        &[
            source("path:a", serde_json::json!({})),
            source("path:b", serde_json::json!({})),
        ],
        3,
        true,
    );
    assert!(!ours.lineages.contains_key(&id), "this pass ended it");
    let changed = login("l2", "rl2", 200);
    let mut theirs = loaded.clone();
    theirs.observe(
        &[source("path:b", serde_json::json!({ "srv": changed }))],
        3,
        false,
    );

    let merged = ours.merged_over(&loaded, &theirs, &rewritten);
    assert!(
        !merged.lineages.contains_key(&id),
        "the signed-out lineage stays ended"
    );
    let new = merged
        .lineage_holding("srv", &fingerprint(&changed).unwrap())
        .expect("the change is kept");
    assert!(!merged.tombstoned(&new), "and reads as a new login");
}

/// A staging file a crashed row write left in the live-session registry is no
/// session's row: it never keeps a pass from reading every source, so a
/// signed-out login with no copy left is forgotten, and a copy put back from
/// outside later reads as the new login it is.
#[test]
fn a_staging_file_in_the_registry_lets_a_tombstone_end() {
    let _home = HomeSandbox::new();
    let l = login("l", "rl", 100);
    seed(&[
        ("alpha", serde_json::json!({ "srv": l })),
        ("beta", serde_json::json!({ "srv": l })),
    ]);
    let registry = crate::live_sessions::registry_dir().expect("registry dir");
    fs::create_dir_all(&registry).expect("registry");
    fs::write(
        registry.join(".6464-0.json.tmp.999.0"),
        b"{\"session_id\": ",
    )
    .expect("a crashed writer's staging file");
    crate::runtime::sync_mcp_logins_once();
    write_mcp("alpha", serde_json::json!({}));
    for _ in 0..3 {
        crate::runtime::sync_mcp_logins_once();
    }
    assert!(
        mcp_of("beta")["srv"].is_null(),
        "beta lost the signed-out login"
    );

    write_mcp("beta", serde_json::json!({ "srv": l }));
    crate::runtime::sync_mcp_logins_once();
    crate::runtime::sync_mcp_logins_once();

    assert_eq!(mcp_of("beta")["srv"], l, "the restored copy stays");
    assert_eq!(
        mcp_of("alpha")["srv"],
        l,
        "and reaches alpha as a new login"
    );
}

/// A Claude Code job list that does not read may hide a job tree holding a
/// copy of a signed-out login, so the daemon pass counts it as a source that
/// did not read and the tombstone stands, even after a sole holder signed out.
#[test]
fn a_job_list_that_does_not_read_keeps_a_tombstone() {
    let home = HomeSandbox::new();
    let l = login("l", "rl", 100);
    seed(&[("one", serde_json::json!({ "srv": l }))]);
    crate::runtime::sync_mcp_logins_once();
    write_mcp("one", serde_json::json!({}));
    let job = home.home().join(".claude").join("jobs").join("0add99ec");
    fs::create_dir_all(&job).expect("job dir");
    fs::write(
        job.join("state.json"),
        br#"{"state":"running","providerEnv":{"CLAUDE_CON"#,
    )
    .expect("torn job record");

    crate::runtime::sync_mcp_logins_once();

    let state = SyncState::load();
    let id = state
        .lineage_holding("srv", &fingerprint(&l).unwrap())
        .expect("the signed-out login is still known");
    assert!(state.tombstoned(&id));
}

// ── the switch carry and the hand-off ───────────────────────────────────────

/// The carry's same-login rule: an account's own login of a server stays
/// where the record knows the carried copy as another login, a copy of the
/// same login is carried, a copy the record cannot place is carried, and a
/// login only the target held stays.
#[test]
fn the_carry_keeps_an_accounts_own_login_and_carries_a_copy_of_the_same_one() {
    let shared_old = login("s1", "rs", 100);
    let shared_new = login("s2", "rs", 300);
    let own = login("own", "r-own", 50);
    let carried = login("theirs", "r-theirs", 900);
    let unknown = login("rotated", "r-rotated", 900);
    let mut state = SyncState::default();
    let before = store_value(
        serde_json::json!({ "srv": own, "shared": shared_old, "only": own, "moved": own }),
    );
    state.observe(
        &[
            Source {
                key: Some("path:target".to_string()),
                value: Some(before.clone()),
            },
            source(
                "path:live",
                serde_json::json!({ "srv": carried, "shared": shared_new }),
            ),
        ],
        1,
        true,
    );
    let mut merged =
        store_value(serde_json::json!({ "srv": carried, "shared": shared_new, "moved": unknown }));

    assert!(state.keep_own_logins(&mut merged, &before, None, Some("path:target")));

    assert_eq!(
        merged["mcpOAuth"]["moved"], unknown,
        "a copy the record cannot place is carried"
    );

    assert_eq!(
        merged["mcpOAuth"]["srv"], own,
        "the account's own login stays"
    );
    assert_eq!(
        merged["mcpOAuth"]["shared"], shared_new,
        "a copy of the same login is carried"
    );
    assert_eq!(
        merged["mcpOAuth"]["only"], own,
        "a login only the target held stays"
    );
}

/// The hand-off carries nothing of a signed-out login while the sync is on.
#[test]
fn the_hand_off_skips_a_tombstoned_lineage() {
    let _home = HomeSandbox::new();
    seed(&[("one", serde_json::json!({}))]);
    let l = login("l", "rl", 100);
    let keep = login("k", "rk", 100);
    let mut state = SyncState::default();
    // The handed-off session's item still holds the signed-out login, and is
    // read by the passes: that copy keeps the tombstone standing.
    let item_source = || {
        source(
            "item:/r/runtime",
            serde_json::json!({ "srv": l, "keep": keep }),
        )
    };
    state.observe(
        &[
            source("path:s", serde_json::json!({ "srv": l, "keep": keep })),
            item_source(),
        ],
        1,
        true,
    );
    state.observe(
        &[
            source("path:s", serde_json::json!({ "keep": keep })),
            item_source(),
        ],
        2,
        true,
    );
    crate::lock::with_state_lock(|_held| {
        crate::profile::atomic_write_600(&state_path()?, serde_json::to_vec(&state)?)
            .map_err(anyhow::Error::from)
    })
    .expect("save the record");
    let item = store_value(serde_json::json!({ "srv": l, "keep": keep }));

    let carried = crate::claude::hand_off_carried_keys(&item).expect("something to carry");

    assert_eq!(
        Value::Object(carried),
        serde_json::json!({ "mcpOAuth": { "keep": keep } })
    );
    let only_signed_out = store_value(serde_json::json!({ "srv": l }));
    assert_eq!(crate::claude::hand_off_carried_keys(&only_signed_out), None);
}
