#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The codex credential engine: the auth model's tolerant reads and
//! key-preserving rotation, the refresh classification, and the standby
//! pass's whole decision table — age gate, memo, kick, stand-down, belt.

use super::*;
use crate::testutil::{
    HomeSandbox, codex_auth_body, jwt_with_exp, read_codex_store, write_codex_store,
};

// Per-test profile NAMES: the attempt memo, kick, and bad-read maps are
// process-global statics keyed by name, and these tests run in parallel.

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
    let auth = CodexAuth::parse(codex_auth_body("at.old", "rt.old").as_bytes()).expect("parse");
    let tok = CodexTokenResponse {
        id_token: Some("id.new".into()),
        access_token: "at.new".into(),
        refresh_token: "rt.new".into(),
    };
    let rotated = auth.with_rotated(&tok, "2026-08-13T00:00:00Z");
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
    // The literal the spec verified against rust-v0.145.0, never the constant
    // compared to itself.
    assert_eq!(body["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
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
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 86_400), "rt.a"),
    );
    assert_eq!(standby_pass(name, now, stamp(), &ok), StandbyOutcome::Idle);

    // Due (inside the lead), parked: rotates, preserves keys, records the belt.
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"),
    );
    assert_eq!(
        standby_pass(name, now, stamp(), &ok),
        StandbyOutcome::Rotated
    );
    let stored = read_codex_store(name);
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
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"),
    );
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
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"),
    );
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
/// one-liner) and to codex's own window, pinned at the BOUNDARY as a literal
/// (300_000 ms, the verified 5 minutes): one second inside stands down, one
/// second outside rotates under the same live session, so a window constant
/// moved by a minute in either direction reds. A parked profile inside the
/// window still rotates — waiting there is how parked chains die at the wham
/// 401.
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
    let exp_secs = |lead_ms: i64| (now + lead_ms) / 1000;

    // A LIVE session holds the chain for the rest of the test.
    let sessions = home.home().join(".clauth/profiles/cx-standdown/sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("99999")).expect("open pid");
    pid.lock().expect("lock pid");

    // One second inside the verified 5-minute window (300_000 ms): stood down.
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp(exp_secs(300_000 - 1_000)), "rt.a"),
    );
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::StoodDown
    );
    assert!(
        read_codex_store(name).contains("rt.a"),
        "a stood-down pass spends nothing"
    );

    // One second outside it, same live session: due (inside the standby lead)
    // and safe, so the rotation runs.
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp(exp_secs(300_000 + 1_000)), "rt.a"),
    );
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Rotated
    );
    drop(pid);
    std::fs::remove_dir_all(&sessions).expect("clear sessions");

    // Inside the window, parked: the rotation runs.
    write_codex_store(
        name,
        &codex_auth_body(
            &jwt_with_exp(exp_secs(CODEX_SELF_REFRESH_WINDOW_MS - 1_000)),
            "rt.a",
        ),
    );
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
    let good = codex_auth_body(&jwt_with_exp((now / 1000) + 86_400), "rt.a");
    write_codex_store(name, &good);
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Idle
    );

    // The store goes bad (a crash mid-truncate). The first strike stamps the
    // clock; a second read microseconds later is NOT confirmation.
    write_codex_store(name, "{ half a wri");
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
        read_codex_store(name),
        "{ half a wri",
        "nothing restored while the bad window is short"
    );
    // Past the confirmation interval, still bad, still parked: restore.
    assert_eq!(
        standby_pass(name, now + 31_000, "2026-08-13T00:00:00Z".into(), &ok),
        StandbyOutcome::Restored
    );
    assert_eq!(
        read_codex_store(name),
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
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"),
    );
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
    write_codex_store(
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
    write_codex_store(
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
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 400), "rt.a"),
    );
    let sessions = home.home().join(".clauth/profiles/cx-fake/sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir sessions");
    let pid = crate::runtime::open_pid_file(&sessions.join("99999")).expect("pid");
    pid.lock().expect("lock");

    let forced = crate::runtime::ForcedFakeLinkMode::new();
    // Even a kick must not force a rotation while a fake-mode carrier is live.
    kick_codex(name);
    let out = standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &boom);
    drop(forced);
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
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 86_400), "rt.a"),
    );
    assert_eq!(
        standby_pass(name, now, "x".into(), &unused),
        StandbyOutcome::Idle
    );
    write_codex_store(name, "{ half a wri");

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
        read_codex_store(name),
        "{ half a wri",
        "the store is left for codex"
    );
    drop(pid);
}

/// A successful rotation does NOT reset the kick breaker — only a successful
/// poll does. Otherwise a non-token 401 (a suspended account) would rotate
/// fine each cycle, reset the breaker, and force-refresh forever.
#[test]
fn a_successful_rotation_does_not_reset_the_breaker() {
    let _home = HomeSandbox::new();
    let name = "cx-breaker";
    let now: i64 = 1_700_000_000_000;
    let ok = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Ok(CodexTokenResponse {
            id_token: None,
            access_token: jwt_with_exp((now / 1000) + 3600),
            refresh_token: "rt.rot".into(),
        })
    };
    let fail = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Err(CodexRefreshError::Transient("stub".into()))
    };

    // Memo-block the routine leg so only a kick can drive a refresh.
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"),
    );
    assert_eq!(
        standby_pass(name, now, "x".into(), &fail),
        StandbyOutcome::Failed
    );

    // Kick 1 → forced attempt → SUCCEEDS. If this wrongly reset the breaker,
    // the strike count would return to 0.
    kick_codex(name);
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"),
    );
    assert_eq!(
        standby_pass(name, now, "x".into(), &ok),
        StandbyOutcome::Rotated
    );

    // Kick 2 → still under the breaker (strikes=2) → forced attempt fires.
    kick_codex(name);
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"),
    );
    assert_eq!(
        standby_pass(name, now, "x".into(), &fail),
        StandbyOutcome::Failed
    );

    // Kick 3 → strikes=3 > breaker → nothing fires. This only holds because
    // the successful rotation at kick 1 did NOT reset the count.
    kick_codex(name);
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"),
    );
    assert_eq!(
        standby_pass(name, now, "x".into(), &fail),
        StandbyOutcome::Idle,
        "a rotation that reset the breaker would let this third kick fire"
    );
    kick_reset(name);
}

/// The post-guard re-read catches a live codex rotating the store in the
/// window between the pre-guard capture and the guarded read — clauth must
/// NOT then spend the token codex just rotated away.
#[test]
fn a_rotation_under_the_guard_window_is_caught() {
    let _home = HomeSandbox::new();
    let name = "cx-reread";
    let now: i64 = 1_700_000_000_000;
    let boom = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        panic!("must not hit the wire — the token changed under the guard");
    };
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"),
    );

    // Between the pre-guard capture of rt.a and the post-guard re-read, a live
    // codex rotates the store to rt.b.
    let n = name.to_string();
    crate::codex_auth::set_pre_reread_hook(
        name,
        std::sync::Arc::new(move || {
            write_codex_store(&n, &codex_auth_body("at.b", "rt.b"));
        }),
    );
    assert_eq!(
        standby_pass(name, now, "x".into(), &boom),
        StandbyOutcome::Idle,
        "a token changed under the guard is not spent"
    );
    assert!(
        read_codex_store(name).contains("rt.b"),
        "the live codex's fresh chain is left untouched"
    );
}

/// A terminal verdict out of the refresher leaves the quarantine record the
/// walk, the feed and the start/switch refusals read; `Transient` leaves none;
/// a rotation that lands clears it. Kinds are the words
/// `classify_refresh_failure` produces, the stamp is the pass's own clock.
#[test]
fn a_terminal_verdict_leaves_a_quarantine_record_and_a_rotation_clears_it() {
    let _home = HomeSandbox::new();
    let name = "cx-quarantine";
    let now: i64 = 1_700_000_000_000;
    let due = || codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a");
    let reused = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Err(CodexRefreshError::Reused)
    };
    let expired = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Err(CodexRefreshError::Dead("expired"))
    };
    let transient = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Err(CodexRefreshError::Transient("HTTP 502".into()))
    };
    let ok = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Ok(CodexTokenResponse {
            id_token: None,
            access_token: jwt_with_exp((now / 1000) + 3600),
            refresh_token: "rt.b".into(),
        })
    };
    let record = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
        .expect("dir")
        .join("auth.quarantine.json");

    // Transient: no record.
    write_codex_store(name, &due());
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &transient),
        StandbyOutcome::Failed
    );
    assert_eq!(
        read_quarantine(name),
        None,
        "a retry-later verdict quarantines nothing"
    );
    assert!(!record.exists());

    // Reused: the record, on disk, with the kind and the pass's stamp.
    kick_codex(name);
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &reused),
        StandbyOutcome::Failed
    );
    assert_eq!(
        read_quarantine(name),
        Some(CodexQuarantine {
            kind: "reused".into(),
            at: "2026-08-13T00:00:00Z".into(),
            token_fingerprint: token_fingerprint("rt.a"),
        })
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&record).expect("record"))
            .expect("json")["kind"],
        "reused",
        "the record is the file beside the store, not a static"
    );
    assert_eq!(
        refuse_if_quarantined(name)
            .expect_err("a quarantined chain refuses")
            .to_string(),
        "'cx-quarantine': codex chain is broken (reused since 2026-08-13T00:00:00Z), \
         run `clauth login cx-quarantine --codex --browser`"
    );
    // A later kick re-hearing the same verdict keeps the first stamp: `since`
    // names when the chain died, not the last time the server said so.
    kick_codex(name);
    assert_eq!(
        standby_pass(name, now, "2026-08-13T06:00:00Z".into(), &reused),
        StandbyOutcome::Failed
    );
    assert_eq!(
        read_quarantine(name).map(|q| q.at),
        Some("2026-08-13T00:00:00Z".to_string())
    );
    kick_reset(name);

    // Dead(expired), on a fresh chain: the kind moves with the verdict.
    clear_quarantine(name);
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.c"),
    );
    assert_eq!(
        standby_pass(name, now, "2026-08-14T00:00:00Z".into(), &expired),
        StandbyOutcome::Failed
    );
    assert_eq!(
        read_quarantine(name),
        Some(CodexQuarantine {
            kind: "expired".into(),
            at: "2026-08-14T00:00:00Z".into(),
            token_fingerprint: token_fingerprint("rt.c"),
        })
    );

    // A rotation that lands retires the verdict.
    kick_codex(name);
    assert_eq!(
        standby_pass(name, now, "2026-08-15T00:00:00Z".into(), &ok),
        StandbyOutcome::Rotated
    );
    assert_eq!(
        read_quarantine(name),
        None,
        "a landed rotation clears the record"
    );
    assert!(!record.exists());
    assert!(refuse_if_quarantined(name).is_ok());
    kick_reset(name);
}

/// The record binds to the token it judged: a fresh chain landing in the
/// store by a path that calls no clear (codex's own `codex login` through a
/// still-linked slot) reads as no verdict, the judged token back in the store
/// reads the record again, a store that cannot be read keeps it standing, and
/// a new chain's death is a new verdict with its own stamp.
#[test]
fn a_quarantine_record_speaks_only_for_the_token_it_judged() {
    let _home = HomeSandbox::new();
    let name = "cx-bound";
    let now: i64 = 1_700_000_000_000;
    let on = |token: &str| codex_auth_body(&jwt_with_exp((now / 1000) + 60), token);
    let reused = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Err(CodexRefreshError::Reused)
    };
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");

    write_codex_store(name, &on("rt.a"));
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &reused),
        StandbyOutcome::Failed
    );
    let verdict = CodexQuarantine {
        kind: "reused".into(),
        at: "2026-08-13T00:00:00Z".into(),
        token_fingerprint: token_fingerprint("rt.a"),
    };
    assert_eq!(read_quarantine(name), Some(verdict.clone()));

    // A fresh chain lands with no clear call: the verdict has no claim on it.
    write_codex_store(name, &on("rt.b"));
    assert_eq!(
        read_quarantine(name),
        None,
        "a verdict about rt.a says nothing about rt.b"
    );
    assert!(refuse_if_quarantined(name).is_ok());
    assert!(
        dir.join("auth.quarantine.json").exists(),
        "the file stays; it is the token that moved"
    );

    // The judged token back in the store: the verdict speaks again.
    write_codex_store(name, &on("rt.a"));
    assert_eq!(read_quarantine(name), Some(verdict.clone()));

    // A store that cannot be read keeps the record standing: the verdict is
    // about a chain that may still be there.
    std::fs::remove_file(dir.join("auth.json")).expect("drop store");
    assert_eq!(read_quarantine(name), Some(verdict));

    // The fresh chain dies too: its own record, its own stamp, not the old
    // chain's `since`.
    write_codex_store(name, &on("rt.b"));
    assert_eq!(
        standby_pass(name, now, "2026-08-20T00:00:00Z".into(), &reused),
        StandbyOutcome::Failed
    );
    assert_eq!(
        read_quarantine(name),
        Some(CodexQuarantine {
            kind: "reused".into(),
            at: "2026-08-20T00:00:00Z".into(),
            token_fingerprint: token_fingerprint("rt.b"),
        })
    );
}

/// Which verdicts quarantine: the three the server spells about the chain
/// (`reused`, `expired`, `invalidated`). An unrecognized 4xx (`rejected`) is
/// a statement about the front door — a WAF 403 here — and keeps what it had
/// before the record existed: the memo blocks the routine leg, a kick forces
/// one attempt, and no record is written on either.
#[test]
fn an_unrecognized_4xx_leaves_no_quarantine_record() {
    assert_eq!(CodexRefreshError::Reused.quarantine_kind(), Some("reused"));
    assert_eq!(
        CodexRefreshError::Dead("expired").quarantine_kind(),
        Some("expired")
    );
    assert_eq!(
        CodexRefreshError::Dead("invalidated").quarantine_kind(),
        Some("invalidated")
    );
    assert_eq!(CodexRefreshError::Dead("rejected").quarantine_kind(), None);
    assert_eq!(
        CodexRefreshError::Transient("HTTP 502".into()).quarantine_kind(),
        None
    );

    let _home = HomeSandbox::new();
    let name = "cx-rejected";
    let now: i64 = 1_700_000_000_000;
    let rejected = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        Err(classify_refresh_failure(403, "<html>Access denied</html>"))
    };
    let record = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
        .expect("dir")
        .join("auth.quarantine.json");

    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.a"),
    );
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &rejected),
        StandbyOutcome::Failed
    );
    assert_eq!(read_quarantine(name), None);
    assert!(
        !record.exists(),
        "a front-door failure is no verdict about the chain"
    );
    assert!(refuse_if_quarantined(name).is_ok());

    // The memo blocks the routine leg; a kick forces one attempt; still no record.
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:01:00Z".into(), &rejected),
        StandbyOutcome::Idle,
        "memo-blocked, not re-sent"
    );
    kick_codex(name);
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:02:00Z".into(), &rejected),
        StandbyOutcome::Failed
    );
    assert!(!record.exists());
    kick_reset(name);
}

/// One normalizer for the plan word wherever it was read: trimmed,
/// lowercased, and an empty word is no word, so the id_token reader falls
/// back on `""` exactly as on an absent claim.
#[test]
fn a_plan_word_is_trimmed_lowercased_and_empty_is_absent() {
    assert_eq!(plan_word(" Plus "), Some("plus".to_string()));
    assert_eq!(plan_word("pro"), Some("pro".to_string()));
    assert_eq!(plan_word(""), None);
    assert_eq!(plan_word("   "), None);

    let with_claim = |plan: &str| {
        let id_token = crate::testutil::codex_jwt(&format!(
            r#"{{"https://api.openai.com/auth":{{"chatgpt_plan_type":"{plan}"}}}}"#
        ));
        CodexAuth::parse(format!(r#"{{"tokens":{{"id_token":"{id_token}"}}}}"#).as_bytes())
            .expect("parses")
    };
    assert_eq!(
        with_claim(" Plus ").id_token_plan(),
        Some("plus".to_string())
    );
    assert_eq!(with_claim("").id_token_plan(), None);
}

/// No token reaches the wire without its memo on disk. A memo path that cannot
/// be written ends the pass before the refresher runs and hands back the kick
/// the pass took; the same fixture, writable, sends exactly once.
#[test]
fn an_unwritable_memo_sends_nothing_and_keeps_the_kick() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _home = HomeSandbox::new();
    let name = "cx-memo-fail";
    let now: i64 = 1_700_000_000_000;
    let calls = AtomicUsize::new(0);
    let counting = |_t: &str| -> std::result::Result<CodexTokenResponse, CodexRefreshError> {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(CodexTokenResponse {
            id_token: None,
            access_token: jwt_with_exp((now / 1000) + 3600),
            refresh_token: "rt.b".into(),
        })
    };

    // A fresh chain (not due) that a 401 kicked: only the forced attempt can
    // drive a refresh, so a consumed-then-lost kick would be observable.
    write_codex_store(
        name,
        &codex_auth_body(&jwt_with_exp((now / 1000) + 86_400), "rt.a"),
    );
    let memo = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
        .expect("dir")
        .join("auth.attempt");
    // A directory where the memo file would land: the atomic rename onto it
    // fails on every platform.
    std::fs::create_dir(&memo).expect("occupy the memo path");
    kick_codex(name);

    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &counting),
        StandbyOutcome::MemoFailed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing went on the wire");
    assert!(
        kick_available(name),
        "the forced attempt is still owed: the kick the pass took is handed back"
    );
    assert!(
        read_codex_store(name).contains("rt.a"),
        "the store is untouched"
    );

    // Writable again: the owed attempt fires once and lands.
    std::fs::remove_dir(&memo).expect("free the memo path");
    assert_eq!(
        standby_pass(name, now, "2026-08-13T00:00:00Z".into(), &counting),
        StandbyOutcome::Rotated
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        !kick_available(name),
        "the attempt that reached the wire consumed the kick"
    );
    kick_reset(name);
}

/// The daemon's leg through its own entry point: `standby_tick` reads the
/// roster itself and runs the production refresher, which the token-URL seam
/// points at a local stub. Every due chain rotates, so a tick that visits only
/// the first member — or none — reds.
#[test]
fn standby_tick_rotates_every_due_chain_through_the_wire() {
    let home = HomeSandbox::new();
    let now: i64 = 1_700_000_000_000;
    let (addr, handle) = crate::testutil::serve_endpoints(2, |_path, i| {
        (
            200,
            format!(
                r#"{{"id_token":"id.n","access_token":"at.n{i}","refresh_token":"rt.new{i}"}}"#
            ),
        )
    });
    // RAII, borrowing the home: a fixture panic below must not leave the
    // override pointing the next test at this port.
    let _token_url = crate::testutil::CodexTokenUrlSandbox::new(&home, &addr);

    crate::testutil::write_codex_roster(&["cx-tick-a", "cx-tick-b"]);
    for name in ["cx-tick-a", "cx-tick-b"] {
        write_codex_store(
            name,
            &codex_auth_body(&jwt_with_exp((now / 1000) + 60), "rt.old"),
        );
    }

    standby_tick(now, "2026-08-13T00:00:00Z");

    let seen = handle.join().expect("join stub");
    assert_eq!(
        seen,
        ["/oauth/token", "/oauth/token"],
        "one refresh per due chain, both to the token endpoint"
    );
    let rotated: std::collections::BTreeSet<String> = ["cx-tick-a", "cx-tick-b"]
        .into_iter()
        .map(|name| {
            let v: serde_json::Value =
                serde_json::from_str(&read_codex_store(name)).expect("store parses");
            assert_eq!(v["last_refresh"], "2026-08-13T00:00:00Z", "{name} stamped");
            v["tokens"]["refresh_token"]
                .as_str()
                .expect("refresh token")
                .to_string()
        })
        .collect();
    assert_eq!(
        rotated,
        ["rt.new0".to_string(), "rt.new1".to_string()]
            .into_iter()
            .collect(),
        "BOTH stores hold a rotated pair, one per wire call"
    );
}
