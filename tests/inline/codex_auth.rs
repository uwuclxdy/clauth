#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The codex credential engine: the auth model's tolerant reads and
//! key-preserving rotation, the refresh classification, and the standby
//! pass's whole decision table — age gate, memo, kick, stand-down, belt.

use super::*;
use crate::testutil::HomeSandbox;

// Per-test profile NAMES: the attempt memo, kick, and bad-read maps are
// process-global statics keyed by name, and these tests run in parallel.

/// A JWT whose payload carries `exp` (epoch seconds) — enough for the
/// unverified schedule read.
fn jwt_with_exp(exp_secs: i64) -> String {
    let payload = crate::oauth_login::base64url_nopad(format!("{{\"exp\":{exp_secs}}}").as_bytes());
    format!("h.{payload}.sig")
}

fn auth_body(access: &str, refresh: &str) -> String {
    format!(
        "{{ \"tokens\": {{\"id_token\": \"id.x\", \"access_token\": \"{access}\", \
         \"refresh_token\": \"{refresh}\", \"account_id\": \"acc\"}}, \"keep_me\": 7 }}"
    )
}

fn write_store(name: &str, body: &str) {
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    std::fs::write(dir.join("auth.json"), body).expect("write store");
}

fn read_store(name: &str) -> String {
    std::fs::read_to_string(
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("dir")
            .join("auth.json"),
    )
    .expect("read store")
}

#[test]
fn jwt_exp_reads_the_payload_and_shrugs_at_garbage() {
    assert_eq!(
        jwt_exp_ms(&jwt_with_exp(1_700_000_000)),
        Some(1_700_000_000_000)
    );
    assert_eq!(jwt_exp_ms("not-a-jwt"), None);
    assert_eq!(jwt_exp_ms("a.!!!!.c"), None);
}

#[test]
fn a_rotation_preserves_every_unknown_key() {
    let auth = CodexAuth::parse(auth_body("at.old", "rt.old").as_bytes()).expect("parse");
    let tok = CodexTokenResponse {
        id_token: Some("id.new".into()),
        access_token: "at.new".into(),
        refresh_token: "rt.new".into(),
    };
    let rotated = auth.with_rotated(&tok, "2026-08-13T00:00:00Z".into());
    let v: serde_json::Value = serde_json::from_slice(&rotated.to_bytes()).expect("reparse");
    assert_eq!(v["tokens"]["access_token"], "at.new");
    assert_eq!(v["tokens"]["refresh_token"], "rt.new");
    assert_eq!(v["tokens"]["id_token"], "id.new");
    assert_eq!(v["tokens"]["account_id"], "acc", "untouched slots survive");
    assert_eq!(v["keep_me"], 7, "unknown top-level keys survive");
    assert_eq!(v["last_refresh"], "2026-08-13T00:00:00Z");
}

#[test]
fn refresh_failures_classify_the_way_codex_does() {
    assert!(matches!(
        classify_refresh_failure(400, r#"{"error":"refresh_token_reused"}"#),
        CodexRefreshError::Reused
    ));
    assert!(matches!(
        classify_refresh_failure(400, r#"{"error":"refresh_token_expired"}"#),
        CodexRefreshError::Dead("expired")
    ));
    assert!(matches!(
        classify_refresh_failure(400, r#"{"error":"refresh_token_invalidated"}"#),
        CodexRefreshError::Dead("invalidated")
    ));
    assert!(matches!(
        classify_refresh_failure(403, "nope"),
        CodexRefreshError::Dead("rejected"),
    ));
    assert!(matches!(
        classify_refresh_failure(429, "slow down"),
        CodexRefreshError::Transient(_)
    ));
    assert!(matches!(
        classify_refresh_failure(502, "bad gateway"),
        CodexRefreshError::Transient(_)
    ));
}

/// The wire shape against a local stub: JSON body carrying the spec's three
/// fields, and the rotated pair parsed back.
#[test]
fn the_refresh_wire_shape_is_the_specs() {
    let (addr, handle) = crate::testutil::serve_endpoints(1, |_path, _i| {
        (
            200,
            r#"{"id_token":"id.n","access_token":"at.n","refresh_token":"rt.n"}"#.to_string(),
        )
    });
    let tok =
        refresh_codex_chain_at(&format!("{addr}/oauth/token"), "rt.old").expect("refresh succeeds");
    assert_eq!(tok.access_token, "at.n");
    assert_eq!(tok.refresh_token, "rt.n");
    let seen = handle.join().expect("join stub");
    assert_eq!(seen, ["/oauth/token"], "one call, to the token endpoint");

    // The body contract, pinned as a value (the stub records paths only):
    // exactly the spec's three fields.
    let body = refresh_request_body("rt.old");
    let obj = body.as_object().expect("object");
    assert_eq!(obj.len(), 3, "exactly the verified fields, nothing extra");
    assert_eq!(body["client_id"], CODEX_CLIENT_ID);
    assert_eq!(body["grant_type"], "refresh_token");
    assert_eq!(body["refresh_token"], "rt.old");
}

/// The standby decision table, driven through one profile with an injected
/// refresher. Each verdict is asserted as [`StandbyOutcome`] AND as its
/// on-disk consequence.
#[test]
fn the_standby_pass_walks_its_decision_table() {
    let _home = HomeSandbox::new();
    let name = "cx-table";
    let now: i64 = 1_700_000_000_000;
    let stamp = || "2026-08-13T00:00:00Z".to_string();
    let ok = |t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        assert_eq!(t, "rt.a", "only the post-guard token feeds the wire");
        Ok(CodexTokenResponse {
            id_token: None,
            access_token: jwt_with_exp((now / 1000) + 3600),
            refresh_token: "rt.b".into(),
        })
    };
    let fail = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Err(CodexRefreshError::Transient("stubbed".into()))
    };

    // Fresh token, parked: not due.
    write_store(
        name,
        &auth_body(&jwt_with_exp((now / 1000) + 86_400), "rt.a"),
    );
    assert_eq!(standby_pass(name, now, stamp(), &ok), StandbyOutcome::Idle);

    // Due (inside the lead), parked: rotates, preserves keys, records the belt.
    write_store(name, &auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"));
    assert_eq!(
        standby_pass(name, now, stamp(), &ok),
        StandbyOutcome::Rotated
    );
    let stored = read_store(name);
    assert!(stored.contains("rt.b") && stored.contains("keep_me"));
    let lkg = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
        .expect("dir")
        .join("auth.lkg.json");
    assert_eq!(
        std::fs::read_to_string(&lkg).expect("lkg"),
        stored,
        "the belt records the rotated store"
    );

    // Due but the refresh fails: Failed, and the SAME token is memo-blocked
    // on the next tick.
    write_store(name, &auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"));
    assert_eq!(
        standby_pass(name, now, stamp(), &fail),
        StandbyOutcome::Failed
    );
    assert_eq!(
        standby_pass(name, now, stamp(), &fail),
        StandbyOutcome::Idle,
        "no replay of a spent attempt on the routine leg"
    );

    // A kick buys exactly one forced retry of that same token…
    kick_codex(name);
    assert_eq!(
        standby_pass(name, now, stamp(), &ok),
        StandbyOutcome::Rotated
    );

    // …and the breaker stops the third consecutive kick.
    write_store(name, &auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"));
    assert_eq!(
        standby_pass(name, now, stamp(), &fail),
        StandbyOutcome::Failed
    );
    kick_codex(name);
    assert_eq!(
        standby_pass(name, now, stamp(), &fail),
        StandbyOutcome::Failed
    );
    kick_codex(name);
    assert_eq!(
        standby_pass(name, now, stamp(), &fail),
        StandbyOutcome::Failed
    );
    kick_codex(name);
    assert_eq!(
        standby_pass(name, now, stamp(), &fail),
        StandbyOutcome::Idle,
        "past two consecutive kicks nothing fires — re-login territory"
    );
    kick_reset(name);
}

/// The stand-down is SCOPED to a live codex session (the #51-accepted
/// one-liner): a parked profile inside codex's own window still rotates —
/// waiting there is how parked chains die at the wham 401.
#[test]
fn the_stand_down_is_scoped_to_a_live_session() {
    let home = HomeSandbox::new();
    let name = "cx-standdown";
    let now: i64 = 1_700_000_000_000;
    let ok = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Ok(CodexTokenResponse {
            id_token: None,
            access_token: jwt_with_exp((now / 1000) + 3600),
            refresh_token: "rt.b".into(),
        })
    };

    // Inside codex's own 5-minute window, with a LIVE session: stood down.
    write_store(name, &auth_body(&jwt_with_exp((now / 1000) + 120), "rt.a"));
    let sessions = home.home().join(".clauth/profiles/cx-standdown/sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("99999")).expect("open pid");
    pid.lock().expect("lock pid");
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::StoodDown
    );
    drop(pid);
    std::fs::remove_dir_all(&sessions).expect("clear sessions");

    // Same window, parked: the rotation runs.
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Rotated
    );
}

/// The belt: a store that reads bad twice in a row across guard acquisitions
/// is restored from the last-known-good copy; one bad read alone is a live
/// in-place write and repairs nothing.
#[test]
fn the_belt_restores_after_two_confirmed_bad_reads() {
    let _home = HomeSandbox::new();
    let name = "cx-belt";
    let now: i64 = 1_700_000_000_000;
    let ok = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Err(CodexRefreshError::Transient("unused".into()))
    };

    // A good pass records the belt.
    let good = auth_body(&jwt_with_exp((now / 1000) + 86_400), "rt.a");
    write_store(name, &good);
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Idle
    );

    // The store goes bad (a crash mid-truncate): first confirmed bad read is
    // a strike, second restores.
    write_store(name, "{ half a wri");
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Idle
    );
    assert_eq!(
        read_store(name),
        "{ half a wri",
        "one strike repairs nothing"
    );
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Restored
    );
    assert_eq!(
        read_store(name),
        good,
        "the belt restored the last good bytes"
    );
}
