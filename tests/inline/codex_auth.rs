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
    // A successful rotation does NOT reset the breaker (only a successful poll
    // does); reset here to test the breaker from a clean count.
    kick_reset(name);

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

/// The belt restores only after the store reads bad continuously for a real
/// wall-clock interval (past any single codex write) with NO live session —
/// two adjacent ticks are not confirmation, so a slow write is never stomped.
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

    // The store goes bad (a crash mid-truncate). The first strike stamps the
    // clock; a second read microseconds later is NOT confirmation.
    write_store(name, "{ half a wri");
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Idle
    );
    assert_eq!(
        standby_pass(name, now + 1, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Idle,
        "two adjacent ticks are not confirmation — a slow write could still land"
    );
    assert_eq!(
        read_store(name),
        "{ half a wri",
        "nothing restored while the bad window is short"
    );
    // Past the confirmation interval, still bad, still parked: restore.
    assert_eq!(
        standby_pass(name, now + 31_000, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Restored
    );
    assert_eq!(
        read_store(name),
        good,
        "the belt restored the last good bytes"
    );
}

/// The no-replay memo is DURABLE: after a failed refresh the fingerprint sits
/// on disk beside the store, so a fresh process (a daemon restart) that
/// forgets every in-memory map still refuses to replay the token — the
/// decision-7 permanent-death hole.
#[test]
fn the_no_replay_memo_survives_on_disk() {
    let _home = HomeSandbox::new();
    let name = "cx-durable";
    let now: i64 = 1_700_000_000_000;
    let fail = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Err(CodexRefreshError::Transient("stub".into()))
    };
    write_store(name, &auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"));
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &fail),
        StandbyOutcome::Failed
    );
    // The fingerprint is on disk — not merely in a static map.
    let memo = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
        .expect("dir")
        .join("auth.attempt");
    assert!(
        memo.exists(),
        "the attempt memo is persisted beside the store"
    );
    assert_eq!(
        std::fs::read_to_string(&memo)
            .expect("read memo")
            .trim()
            .len(),
        16,
        "an 8-byte fingerprint, hex"
    );
    // A capture/login installing a fresh chain retires the memo.
    crate::codex_auth::forget_attempt(name);
    assert!(!memo.exists());
}

/// An UNREADABLE access-token exp does not make the chain due every tick: it
/// falls back to last_refresh age (codex's own 8-day interval), so a
/// recently-refreshed chain with a non-JWT token is NOT rotated.
#[test]
fn an_unreadable_exp_falls_back_to_last_refresh_age() {
    let _home = HomeSandbox::new();
    let name = "cx-noexp";
    let now: i64 = 1_700_000_000_000;
    let boom = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        panic!("must not hit the wire — not due");
    };
    // Non-JWT access token, last_refresh one hour ago: not due.
    let recent = chrono::DateTime::from_timestamp_millis(now - 3_600_000)
        .expect("ts")
        .to_rfc3339();
    write_store(
        name,
        &format!(
            "{{ \"tokens\": {{\"access_token\": \"not-a-jwt\", \"refresh_token\": \"rt.a\", \
             \"account_id\": \"acc\"}}, \"last_refresh\": \"{recent}\" }}"
        ),
    );
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &boom),
        StandbyOutcome::Idle,
        "a fresh chain with an unreadable exp is not due every tick"
    );

    // last_refresh nine days ago: now due, rotates.
    let old = chrono::DateTime::from_timestamp_millis(now - 9 * 24 * 3_600_000)
        .expect("ts")
        .to_rfc3339();
    write_store(
        name,
        &format!(
            "{{ \"tokens\": {{\"access_token\": \"not-a-jwt\", \"refresh_token\": \"rt.a\", \
             \"account_id\": \"acc\"}}, \"last_refresh\": \"{old}\" }}"
        ),
    );
    let ok = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Ok(CodexTokenResponse {
            id_token: None,
            access_token: "not-a-jwt-2".into(),
            refresh_token: "rt.b".into(),
        })
    };
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Rotated,
        "past the 8-day fallback it rotates"
    );
}

/// Under the fake transport a live session holds a SEPARATE copy of the
/// chain, so the standby stands down for ANY live session — not only inside
/// codex's 5-minute window — and that stand-down burns no kick.
#[test]
fn fake_transport_stands_down_for_any_live_session_and_keeps_the_kick() {
    let home = HomeSandbox::new();
    let name = "cx-fake";
    let now: i64 = 1_700_000_000_000;
    let boom = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        panic!("must not rotate a fake-mode live carrier");
    };
    // Due (inside the lead) but NOT inside codex's own 5-min window.
    write_store(name, &auth_body(&jwt_with_exp((now / 1000) + 400), "rt.a"));
    let sessions = home.home().join(".clauth/profiles/cx-fake/sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("99999")).expect("pid");
    pid.lock().expect("lock");

    crate::runtime::force_fake_link_mode();
    // Even a kick must not force a rotation while a fake-mode carrier is live.
    kick_codex(name);
    let out = standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &boom);
    crate::runtime::clear_forced_link_mode();
    drop(pid);
    assert_eq!(out, StandbyOutcome::StoodDown);
    // The kick was not consumed by the stand-down.
    assert!(
        kick_available(name),
        "a stand-down must not burn the one forced attempt"
    );
    kick_reset(name);
}

/// A live codex session BLOCKS the belt restore: the session is the writer,
/// and stomping its in-place write with the pre-rotation belt is the one path
/// where the belt is worse than doing nothing (it resurrects a spent token).
#[test]
fn the_belt_never_restores_under_a_live_session() {
    let home = HomeSandbox::new();
    let name = "cx-belt-live";
    let now: i64 = 1_700_000_000_000;
    let unused = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Err(CodexRefreshError::Transient("unused".into()))
    };
    // Record a belt from a good read, then the store goes bad.
    write_store(
        name,
        &auth_body(&jwt_with_exp((now / 1000) + 86_400), "rt.a"),
    );
    assert_eq!(
        standby_pass(name, now, "x".into(), &unused),
        StandbyOutcome::Idle
    );
    write_store(name, "{ half a wri");

    // A live session — codex is the writer.
    let sessions = home.home().join(".clauth/profiles/cx-belt-live/sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    let pid = crate::runtime::open_pid_file(&sessions.join("1")).expect("pid");
    pid.lock().expect("lock");

    // Even well past the confirmation interval, the restore is refused.
    assert_eq!(
        standby_pass(name, now + 1, "x".into(), &unused),
        StandbyOutcome::Idle
    );
    assert_eq!(
        standby_pass(name, now + 120_000, "x".into(), &unused),
        StandbyOutcome::Idle,
        "a live session is the writer — never stomp its in-place write"
    );
    assert_eq!(
        read_store(name),
        "{ half a wri",
        "the store is left for codex"
    );
    drop(pid);
}
