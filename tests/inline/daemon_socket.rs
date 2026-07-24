#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Control-socket command routing (`dispatch`), exercised directly so no real
//! `UnixListener` is needed. Each command's effect is asserted on the shared
//! stores it enqueues into.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use super::*;
use crate::lockorder::RankedMutex;
use crate::profile::{AppConfig, AppState, Profile};
use crate::testutil::HomeSandbox;

/// Whether the pending-switch queue holds a request for `target` (TECH-6: the
/// queue is now ordered `PendingSwitchEntry`s, not a `HashSet<String>`).
fn switch_queued(h: &SocketHandles, target: &str) -> bool {
    h.pending_switch
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.target == target)
}

fn handles(names: &[&str]) -> SocketHandles {
    let profiles: Vec<Profile> = names
        .iter()
        .map(|n| Profile::new(n.to_string(), None, None))
        .collect();
    let config = AppConfig {
        state: AppState::default(),
        profiles,
    };
    SocketHandles {
        config: Arc::new(RankedMutex::new(config)),
        pending_switch: Arc::new(RankedMutex::new(VecDeque::new())),
        pending_config_ops: Arc::new(RankedMutex::new(Vec::new())),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        waker: Arc::new(crate::daemon::waker::TickWaker::default()),
    }
}

/// The single config op the queue holds, or `None` — every valid config command
/// enqueues exactly one.
fn only_op(h: &SocketHandles) -> Option<ConfigOp> {
    let q = h.pending_config_ops.lock().unwrap();
    (q.len() == 1).then(|| q[0].clone())
}

fn no_status() -> std::path::PathBuf {
    std::path::PathBuf::from("/nonexistent/status.json")
}

#[test]
fn switch_valid_profile_enqueues_and_acks() {
    let _home = HomeSandbox::new();
    let h = handles(&["work", "home"]);
    // Case-insensitive resolution → canonical name enqueued.
    let resp = dispatch(r#"{"cmd":"switch","profile":"WORK"}"#, &no_status(), &h);
    assert_eq!(resp, "{\"ok\":true}");
    assert!(switch_queued(&h, "work"));
}

#[test]
fn switch_unknown_profile_errors_and_enqueues_nothing() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    let resp = dispatch(r#"{"cmd":"switch","profile":"nope"}"#, &no_status(), &h);
    assert!(resp.contains("\"ok\":false"));
    assert!(resp.contains("unknown profile"));
    assert!(h.pending_switch.lock().unwrap().is_empty());
}

#[test]
fn switch_without_profile_errors() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    let resp = dispatch(r#"{"cmd":"switch"}"#, &no_status(), &h);
    assert!(resp.contains("\"ok\":false"));
    assert!(h.pending_switch.lock().unwrap().is_empty());
}

#[test]
fn refresh_all_enqueues_every_profile() {
    let _home = HomeSandbox::new();
    let h = handles(&["a", "b"]);
    let resp = dispatch(r#"{"cmd":"refresh"}"#, &no_status(), &h);
    assert_eq!(resp, "{\"ok\":true}");
    let q = h.refetch_queue.lock().unwrap();
    assert!(q.contains("a") && q.contains("b"));
}

#[test]
fn refresh_one_enqueues_only_that_profile() {
    let _home = HomeSandbox::new();
    let h = handles(&["a", "b"]);
    let resp = dispatch(r#"{"cmd":"refresh","profile":"a"}"#, &no_status(), &h);
    assert_eq!(resp, "{\"ok\":true}");
    let q = h.refetch_queue.lock().unwrap();
    assert!(q.contains("a") && !q.contains("b"));
}

#[test]
fn unknown_cmd_and_malformed_json_error() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    assert!(dispatch(r#"{"cmd":"frobnicate"}"#, &no_status(), &h).contains("unknown cmd"));
    assert!(dispatch("not json", &no_status(), &h).contains("bad command"));
}

#[test]
fn fallback_add_remove_enqueue_canonical_name() {
    let _home = HomeSandbox::new();
    let h = handles(&["work", "home"]);
    // Case-insensitive resolve → canonical name in the op.
    let resp = dispatch(
        r#"{"cmd":"fallback_add","profile":"WORK"}"#,
        &no_status(),
        &h,
    );
    assert_eq!(resp, "{\"ok\":true}");
    assert_eq!(only_op(&h), Some(ConfigOp::FallbackAdd("work".into())));

    let h = handles(&["work"]);
    dispatch(
        r#"{"cmd":"fallback_remove","profile":"work"}"#,
        &no_status(),
        &h,
    );
    assert_eq!(only_op(&h), Some(ConfigOp::FallbackRemove("work".into())));
}

#[test]
fn fallback_config_unknown_profile_errors_and_enqueues_nothing() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    let resp = dispatch(
        r#"{"cmd":"fallback_add","profile":"nope"}"#,
        &no_status(),
        &h,
    );
    assert!(resp.contains("\"ok\":false") && resp.contains("unknown profile"));
    assert!(h.pending_config_ops.lock().unwrap().is_empty());
}

#[test]
fn fallback_move_parses_dir_and_rejects_bad_dir() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    let resp = dispatch(
        r#"{"cmd":"fallback_move","profile":"work","dir":"UP"}"#,
        &no_status(),
        &h,
    );
    assert_eq!(resp, "{\"ok\":true}");
    assert_eq!(
        only_op(&h),
        Some(ConfigOp::FallbackMove("work".into(), MoveDir::Up))
    );

    let h = handles(&["work"]);
    let resp = dispatch(
        r#"{"cmd":"fallback_move","profile":"work","dir":"sideways"}"#,
        &no_status(),
        &h,
    );
    assert!(resp.contains("\"ok\":false"));
    assert!(h.pending_config_ops.lock().unwrap().is_empty());
}

#[test]
fn set_last_resort_validates_bool_and_enqueues() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    let resp = dispatch(
        r#"{"cmd":"set_last_resort","profile":"work","value":true}"#,
        &no_status(),
        &h,
    );
    assert_eq!(resp, "{\"ok\":true}");
    assert_eq!(
        only_op(&h),
        Some(ConfigOp::SetLastResort("work".into(), true))
    );

    for bad in [
        r#"{"cmd":"set_last_resort","profile":"work","value":1}"#,
        r#"{"cmd":"set_last_resort","profile":"work","value":"yes"}"#,
        r#"{"cmd":"set_last_resort","profile":"work"}"#,
        r#"{"cmd":"set_last_resort","value":true}"#,
        r#"{"cmd":"set_last_resort","profile":"ghost","value":true}"#,
    ] {
        let h = handles(&["work"]);
        assert!(
            dispatch(bad, &no_status(), &h).contains("\"ok\":false"),
            "{bad}"
        );
        assert!(h.pending_config_ops.lock().unwrap().is_empty(), "{bad}");
    }
}

#[test]
fn set_threshold_validates_range_and_type() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    let resp = dispatch(
        r#"{"cmd":"set_threshold","profile":"work","value":90}"#,
        &no_status(),
        &h,
    );
    assert_eq!(resp, "{\"ok\":true}");
    assert_eq!(
        only_op(&h),
        Some(ConfigOp::SetThreshold("work".into(), 90.0))
    );

    for bad in [
        r#"{"cmd":"set_threshold","profile":"work","value":150}"#,
        r#"{"cmd":"set_threshold","profile":"work","value":-1}"#,
        r#"{"cmd":"set_threshold","profile":"work","value":"nope"}"#,
        r#"{"cmd":"set_threshold","profile":"work"}"#,
    ] {
        let h = handles(&["work"]);
        assert!(
            dispatch(bad, &no_status(), &h).contains("\"ok\":false"),
            "{bad}"
        );
        assert!(h.pending_config_ops.lock().unwrap().is_empty(), "{bad}");
    }
}

#[test]
fn set_wrap_off_requires_bool() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    let resp = dispatch(r#"{"cmd":"set_wrap_off","value":true}"#, &no_status(), &h);
    assert_eq!(resp, "{\"ok\":true}");
    assert_eq!(only_op(&h), Some(ConfigOp::SetWrapOff(true)));

    let h = handles(&["work"]);
    assert!(
        dispatch(r#"{"cmd":"set_wrap_off","value":7}"#, &no_status(), &h).contains("\"ok\":false")
    );
    assert!(h.pending_config_ops.lock().unwrap().is_empty());
}

#[test]
fn set_weekly_threshold_validates_range_on_the_socket() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    let resp = dispatch(
        r#"{"cmd":"set_weekly_threshold","value":95}"#,
        &no_status(),
        &h,
    );
    assert_eq!(resp, "{\"ok\":true}");
    assert_eq!(only_op(&h), Some(ConfigOp::SetWeeklyThreshold(95.0)));

    // Out-of-band / non-numeric values error on the socket, enqueue nothing.
    for bad in [
        r#"{"cmd":"set_weekly_threshold","value":49}"#,
        r#"{"cmd":"set_weekly_threshold","value":101}"#,
        r#"{"cmd":"set_weekly_threshold","value":"98"}"#,
        r#"{"cmd":"set_weekly_threshold"}"#,
    ] {
        let h = handles(&["work"]);
        assert!(
            dispatch(bad, &no_status(), &h).contains("\"ok\":false"),
            "{bad}"
        );
        assert!(h.pending_config_ops.lock().unwrap().is_empty(), "{bad}");
    }
}

// ── AUTH-2: stable error_code + synchronous auth_broken refusal ───────────────

#[test]
fn error_replies_carry_stable_error_code() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    // unknown profile → unknown_profile
    let r = dispatch(r#"{"cmd":"switch","profile":"nope"}"#, &no_status(), &h);
    assert!(r.contains("\"error_code\":\"unknown_profile\""), "got: {r}");
    // out-of-vocabulary value → invalid_value
    let r = dispatch(r#"{"cmd":"set_wrap_off","value":7}"#, &no_status(), &h);
    assert!(r.contains("\"error_code\":\"invalid_value\""), "got: {r}");
    // malformed JSON → invalid_value
    let r = dispatch("not json", &no_status(), &h);
    assert!(r.contains("\"error_code\":\"invalid_value\""), "got: {r}");
}

#[test]
fn switch_to_auth_broken_profile_refused_with_code() {
    let _home = HomeSandbox::new();
    let h = handles(&["work", "home"]);
    h.config.lock().unwrap().set_auth_broken("work", true);

    let resp = dispatch(r#"{"cmd":"switch","profile":"work"}"#, &no_status(), &h);
    assert!(resp.contains("\"ok\":false"), "got: {resp}");
    assert!(
        resp.contains("\"error_code\":\"auth_broken\""),
        "got: {resp}"
    );
    assert!(
        resp.contains("clauth login work"),
        "prose login hint: {resp}"
    );
    assert!(
        h.pending_switch.lock().unwrap().is_empty(),
        "a broken target must not enqueue a switch"
    );

    // A healthy target still switches normally.
    let resp2 = dispatch(r#"{"cmd":"switch","profile":"home"}"#, &no_status(), &h);
    assert_eq!(resp2, "{\"ok\":true}");
    assert!(switch_queued(&h, "home"));
}

// ── TECH-10: socket I/O hardening (read timeout + per-connection isolation) ───

/// A connection that opens and never sends a newline must NOT wedge the accept
/// loop: with a per-connection thread + read timeout, a second connection's valid
/// command is still served promptly. Pre-TECH-10 (inline handle, untimed
/// `read_line`) the first connection parked the loop forever and this hung.
#[test]
fn hung_connection_read_timeout_does_not_block_accept_loop() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("clauthd.sock");
    let status = no_status();
    let h = handles(&["work"]);

    let sock_srv = sock.clone();
    std::thread::spawn(move || {
        // Detached: serve() loops on accept forever; it dies when the test binary
        // exits. The tempdir socket keeps it off the real ~/.clauth path.
        let _ = serve(&sock_srv, &status, &h);
    });

    // Wait for the listener to bind (poll the socket path — no fixed sleep).
    for _ in 0..500 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(sock.exists(), "serve() never bound the socket");

    // conn1: connect and deliberately send NOTHING (no newline). Held open for the
    // duration of the test to prove it can't serialize the accept loop.
    let _slow = UnixStream::connect(&sock).expect("connect conn1 (the hung one)");

    // conn2: a valid command must still get its reply promptly, despite conn1.
    let mut fast = UnixStream::connect(&sock).expect("connect conn2");
    fast.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    fast.write_all(b"{\"cmd\":\"switch\",\"profile\":\"work\"}\n")
        .expect("write conn2 command");
    let mut resp = String::new();
    BufReader::new(&fast)
        .read_line(&mut resp)
        .expect("conn2 must receive a reply while conn1 hangs");
    assert_eq!(
        resp.trim(),
        "{\"ok\":true}",
        "the second connection is served despite a hung first one"
    );
}

/// The reply must be ONE line even though status.json on disk is
/// pretty-printed (the daemon writes `to_vec_pretty`) — the protocol is
/// newline-delimited, so a multi-line embed hands `read_line` clients
/// truncated JSON. The fixture is REAL pretty output, not hand-compacted.
#[test]
fn snapshot_reply_is_one_line_around_a_pretty_status_file() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    let dir = crate::profile::clauth_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let status_path = dir.join("status.json");
    let status = serde_json::json!({"schema": 1, "profiles": [{"name": "work"}]});
    std::fs::write(&status_path, serde_json::to_vec_pretty(&status).unwrap()).unwrap();
    let resp = dispatch(r#"{"cmd":"snapshot"}"#, &status_path, &h);
    assert!(
        !resp.contains('\n'),
        "reply must be a single protocol line: {resp:?}"
    );
    let v: serde_json::Value = serde_json::from_str(&resp).expect("reply parses");
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], status, "status round-trips through the reply");
}

#[test]
fn rename_valid_enqueues_canonical_op_and_acks() {
    let _home = HomeSandbox::new();
    let h = handles(&["work", "home"]);
    // Case-insensitive resolve of the OLD name → canonical in the op; the new name
    // is trimmed and carried verbatim.
    let resp = dispatch(
        r#"{"cmd":"rename","profile":"WORK","new_name":" work2 "}"#,
        &no_status(),
        &h,
    );
    assert_eq!(resp, "{\"ok\":true}");
    assert_eq!(
        only_op(&h),
        Some(ConfigOp::Rename("work".into(), "work2".into()))
    );
}

#[test]
fn rename_to_a_taken_name_errors_and_enqueues_nothing() {
    let _home = HomeSandbox::new();
    let h = handles(&["work", "home"]);
    let resp = dispatch(
        r#"{"cmd":"rename","profile":"work","new_name":"home"}"#,
        &no_status(),
        &h,
    );
    assert!(resp.contains("already exists"), "collision refused: {resp}");
    assert_eq!(only_op(&h), None, "no op enqueued on a rejected rename");
}

#[test]
fn rename_unknown_profile_or_missing_new_name_errors() {
    let _home = HomeSandbox::new();
    let h = handles(&["work"]);
    assert!(
        dispatch(
            r#"{"cmd":"rename","profile":"nope","new_name":"x"}"#,
            &no_status(),
            &h
        )
        .contains("unknown_profile")
    );
    assert!(
        dispatch(r#"{"cmd":"rename","profile":"work"}"#, &no_status(), &h)
            .contains("requires new_name")
    );
    assert_eq!(only_op(&h), None);
}

#[test]
fn per_member_weekly_and_gate_commands_validate_and_enqueue() {
    let _home = HomeSandbox::new();

    // Override set, cleared via explicit null, and cleared via absent value.
    let h = handles(&["work"]);
    assert_eq!(
        dispatch(
            r#"{"cmd":"set_member_weekly","profile":"work","value":90}"#,
            &no_status(),
            &h,
        ),
        "{\"ok\":true}"
    );
    assert_eq!(
        only_op(&h),
        Some(ConfigOp::SetMemberWeekly("work".into(), Some(90.0)))
    );
    for clear in [
        r#"{"cmd":"set_member_weekly","profile":"work","value":null}"#,
        r#"{"cmd":"set_member_weekly","profile":"work"}"#,
    ] {
        let h = handles(&["work"]);
        assert_eq!(
            dispatch(clear, &no_status(), &h),
            "{\"ok\":true}",
            "{clear}"
        );
        assert_eq!(
            only_op(&h),
            Some(ConfigOp::SetMemberWeekly("work".into(), None)),
            "{clear}"
        );
    }

    // The two gates route to their scoped/weekly halves.
    let h = handles(&["work"]);
    assert_eq!(
        dispatch(
            r#"{"cmd":"set_check_weekly","profile":"work","value":false}"#,
            &no_status(),
            &h,
        ),
        "{\"ok\":true}"
    );
    assert_eq!(
        only_op(&h),
        Some(ConfigOp::SetUsageGate("work".into(), false, false))
    );
    let h = handles(&["work"]);
    assert_eq!(
        dispatch(
            r#"{"cmd":"set_check_scoped","profile":"work","value":true}"#,
            &no_status(),
            &h,
        ),
        "{\"ok\":true}"
    );
    assert_eq!(
        only_op(&h),
        Some(ConfigOp::SetUsageGate("work".into(), true, true))
    );

    // Rejections error on the socket and enqueue nothing.
    for bad in [
        r#"{"cmd":"set_member_weekly","profile":"work","value":150}"#,
        r#"{"cmd":"set_member_weekly","profile":"work","value":"90"}"#,
        r#"{"cmd":"set_member_weekly","value":90}"#,
        r#"{"cmd":"set_member_weekly","profile":"ghost","value":90}"#,
        r#"{"cmd":"set_check_weekly","profile":"work","value":1}"#,
        r#"{"cmd":"set_check_scoped","profile":"work"}"#,
        r#"{"cmd":"set_check_scoped","value":true}"#,
    ] {
        let h = handles(&["work"]);
        assert!(
            dispatch(bad, &no_status(), &h).contains("\"ok\":false"),
            "{bad}"
        );
        assert!(h.pending_config_ops.lock().unwrap().is_empty(), "{bad}");
    }
}
