//! Inline tests for the generic usage engine — the JSON scanner
//! (bars/rows/plan), error-envelope rejection, and the one network leg worth a
//! listener (the 401 early-abort). Everything else `fetch` does is exercised
//! manually, not here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

// Real z.ai `/api/monitor/usage/quota/limit` shape (trimmed).
const ZAI_QUOTA: &str = r#"{
    "code":200,"msg":"Operation successful","success":true,
    "data":{"level":"pro","limits":[
        {"type":"TIME_LIMIT","percentage":0,"nextResetTime":1784489490994,
         "usage":1000,"currentValue":0,"remaining":1000},
        {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":1,"nextResetTime":1781915527377}
    ]}
}"#;

#[test]
fn scan_zai_quota_shape_yields_bars_and_plan() {
    let value: serde_json::Value = serde_json::from_str(ZAI_QUOTA).unwrap();
    assert!(!is_error_envelope(&value));

    let (plan, bars, rows) = scan(&value);
    assert_eq!(plan.as_deref(), Some("pro"));
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].label, "time limit");
    assert_eq!(bars[0].pct, 0.0);
    assert!(bars[0].resets_at.is_some());
    // Absolute amounts: `currentValue` → used, `used + remaining` → total (no
    // explicit ceiling field). Rendered as the bar's trailing `x / y`.
    assert_eq!(bars[0].used, Some(0.0));
    assert_eq!(bars[0].total, Some(1000.0));
    assert_eq!(bars[1].label, "tokens limit");
    assert_eq!(bars[1].pct, 1.0);
    // Percentage-only limit carries no absolute amounts.
    assert!(bars[1].used.is_none() && bars[1].total.is_none());
    assert!(rows.is_empty(), "bars present → no scalar rows harvested");
}

#[test]
fn scan_zai_200_error_envelope_is_rejected() {
    // z.ai returns this 200 body for unknown routes — must not parse as empty usage.
    let value: serde_json::Value =
        serde_json::from_str(r#"{"code":500,"msg":"404 NOT_FOUND","success":false}"#).unwrap();
    assert!(is_error_envelope(&value));
    let (plan, bars, rows) = scan(&value);
    assert!(plan.is_none() && bars.is_empty() && rows.is_empty());
}

#[test]
fn scan_scalar_balance_shape_yields_rows_not_bars() {
    // A provider returning balances (no percentages) → text rows.
    let body = r#"{"is_available":true,"balance_infos":[
        {"currency":"USD","total_balance":12.5,"granted_balance":5.0,"topped_up_balance":7.5}
    ]}"#;
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    assert!(!is_error_envelope(&value));

    let (plan, bars, rows) = scan(&value);
    assert!(bars.is_empty(), "no percentage field → no bars");
    assert!(plan.is_none());
    let values: Vec<&str> = rows.iter().map(|r| r.value.as_str()).collect();
    assert!(values.contains(&"12.50"));
    assert!(values.contains(&"7.50"));
    assert!(values.contains(&"5"));
}

#[test]
fn humanize_label_handles_cases() {
    assert_eq!(humanize_label("TIME_LIMIT"), "time limit");
    assert_eq!(humanize_label("modelCode"), "model code");
    assert_eq!(humanize_label("total_balance"), "total balance");
}

/// A loopback listener answering up to `n` requests with 401. The request
/// count pins the probe's walk: an early abort leaves requests unserved.
///
/// The accept is BLOCKING, so the server thread sleeps in the kernel until
/// the client connects. It still needs CPU to answer, and a loaded windows
/// runner can starve it past the client's own budget (4 s connect + 8 s to
/// response headers): the client reads the hint as `Network`, walks the
/// candidates, and ends in `Status` for a probe that was never wrong. No
/// server-side deadline fixes that race; it only kept the listener alive
/// while the client's own timeouts decide the verdict. The hang guard moved
/// to `join_served`, and the hint test retries a `Status` verdict.
fn serve_401s(n: usize) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = format!("http://{}", listener.local_addr().expect("local addr"));
    let server = std::thread::spawn(move || {
        let mut served = 0;
        while served < n {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let _ = std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            served += 1;
        }
    });
    (addr, server)
}

/// Join a `serve_401s` thread with a deadline. A server that served its
/// quota exits on its own, so the healthy join completes at once. The
/// deadline bounds only the regression case where the probe stops
/// connecting: the blocked `accept` would hold the join forever. That case
/// skips the join and the caller's verdict check still reds on a wrong
/// outcome; the abandoned thread dies with the process.
fn join_served(server: std::thread::JoinHandle<()>, answered: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !server.is_finished() {
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    server.join().expect(answered);
}

/// A HINT 401 is the key's verdict: that endpoint worked before, so its answer
/// is about the credential, not the route. The prober stops on the FIRST
/// request and hands the caller `AuthExpired` — which suppresses the profile —
/// rather than walking the rest of the list.
///
/// The verdict needs one delivered 401, and the delivery races the client's
/// timeouts on a loaded runner (see `serve_401s`). A `Status` verdict here
/// can only mean the 401 never arrived, so a bounded retry is exact: a real
/// regression that turned hint 401s into misses still reds on every attempt.
#[test]
fn a_hint_401_stops_the_probe_and_reads_auth_expired() {
    let mut last = None;
    for _ in 0..3 {
        let (addr, server) = serve_401s(1);
        let err = fetch(&addr, "sk-dead", Some("/api/usage")).expect_err("the key is dead");
        join_served(server, "the listener answered once");
        if matches!(err, ThirdPartyError::AuthExpired) {
            return;
        }
        last = Some(err);
    }
    panic!(
        "the hint 401 never read as auth-expired: {:?}",
        last.expect("every attempt produced an error")
    );
}

/// A CANDIDATE 401 is just another miss: hosts that 401 unmatched routes
/// exist, and reading one as a dead key would write a durable AuthExpired
/// record a key re-entry cannot clear (the fingerprint hashes the key). With
/// no hint the prober walks the whole candidate list and reports the generic
/// failure, which suppresses as Failed.
#[test]
fn a_candidate_401_is_just_another_miss() {
    let (addr, server) = serve_401s(CANDIDATE_PATHS.len());
    let err = fetch(&addr, "sk-dead", None).expect_err("no candidate worked");
    join_served(server, "every candidate was answered");
    assert!(matches!(err, ThirdPartyError::Status), "got {err:?}");
}
