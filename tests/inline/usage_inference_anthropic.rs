use std::time::Duration;

use super::{MODEL, ProbeAnswer, answer_from, probe, reading_from_headers};
use crate::testutil::{
    EndpointSandbox, HomeSandbox, request_body, request_header, serve_endpoints_raw_with_headers,
    window_exhaustion_429_headers,
};
use crate::usage::inference::{InferenceReading, InferenceWindow, LimitStatus};
use crate::usage::{UsageInfo, iso_to_epoch_secs};

fn read(pairs: &[(&str, &str)]) -> Option<InferenceReading> {
    reading_from_headers(pairs.iter().copied())
}

fn names(reading: &InferenceReading) -> Vec<&str> {
    reading.windows.iter().map(|w| w.name.as_str()).collect()
}

fn window<'r>(reading: &'r InferenceReading, name: &str) -> &'r InferenceWindow {
    reading
        .windows
        .iter()
        .find(|w| w.name == name)
        .unwrap_or_else(|| panic!("no {name} window in {reading:?}"))
}

/// The window headers of the 2026-10-07 Fable probe from #110, with the values
/// it returned (5h reset 18:30 UTC, 7d reset Saturday 16:00 UTC). The response
/// carried more headers than these.
const FABLE_PROBE: &[(&str, &str)] = &[
    ("anthropic-ratelimit-unified-5h-reset", "1791397800"),
    ("anthropic-ratelimit-unified-5h-status", "allowed"),
    ("anthropic-ratelimit-unified-5h-utilization", "0.0"),
    ("anthropic-ratelimit-unified-7d-reset", "1791648000"),
    ("anthropic-ratelimit-unified-7d-status", "allowed"),
    ("anthropic-ratelimit-unified-7d-utilization", "0.2"),
    ("anthropic-ratelimit-unified-7d_oi-reset", "1791648000"),
    ("anthropic-ratelimit-unified-7d_oi-status", "allowed"),
    ("anthropic-ratelimit-unified-7d_oi-utilization", "0.0"),
    (
        "anthropic-ratelimit-unified-representative-claim",
        "five_hour",
    ),
    ("anthropic-ratelimit-unified-status", "allowed"),
    ("anthropic-ratelimit-unified-reset", "1791397800"),
];

#[test]
fn a_fable_probe_reads_5h_7d_and_7d_oi() {
    let reading = read(FABLE_PROBE).expect("reading");
    assert_eq!(names(&reading), ["5h", "7d", "7d_oi"]);
    assert_eq!(
        window(&reading, "5h"),
        &InferenceWindow {
            name: "5h".into(),
            utilization: 0.0,
            resets_at: Some(1_791_397_800),
            status: Some(LimitStatus::Allowed),
        }
    );
    let week = window(&reading, "7d");
    assert_eq!(week.utilization, 20.0);
    assert_eq!(week.resets_at, Some(1_791_648_000));
    assert_eq!(window(&reading, "7d_oi").resets_at, Some(1_791_648_000));
    assert_eq!(reading.status, Some(LimitStatus::Allowed));
    assert_eq!(reading.resets_at, Some(1_791_397_800));
}

#[test]
fn a_haiku_probe_has_no_7d_oi() {
    let haiku: Vec<(&str, &str)> = FABLE_PROBE
        .iter()
        .copied()
        .filter(|(name, _)| !name.contains("7d_oi"))
        .collect();
    let reading = read(&haiku).expect("reading");
    assert_eq!(names(&reading), ["5h", "7d"]);
}

/// The recorded window-exhaustion 429 (`testutil::window_exhaustion_429_headers`)
/// is a reading too: a spent 5h window the server rejects on, beside a week
/// that still allows. Its `overage-*` fields carry no utilization, so they are
/// no window.
#[test]
fn a_window_exhaustion_429_is_a_reading() {
    let headers = crate::testutil::window_exhaustion_429_headers(1_791_400_000, 1_791_700_000);
    let reading = reading_from_headers(headers.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .expect("reading");
    assert_eq!(names(&reading), ["5h", "7d"]);
    let five = window(&reading, "5h");
    assert_eq!(five.utilization, 100.0);
    assert_eq!(five.resets_at, Some(1_791_400_000));
    assert_eq!(five.status, Some(LimitStatus::Rejected));
    let week = window(&reading, "7d");
    assert_eq!(week.utilization, 62.0, "0.62 reads as exactly 62");
    assert_eq!(week.resets_at, Some(1_791_700_000));
    assert_eq!(week.status, Some(LimitStatus::Allowed));
    assert_eq!(reading.status, Some(LimitStatus::Rejected));
    assert_eq!(reading.resets_at, Some(1_791_400_000));
}

/// `slow-budget`, the window Claude Code 2.1.294 reads off a low-priority
/// request and no recorded response has carried yet, parses with no code
/// change: nothing in the parse names it. The lane's own state
/// (`slow-status`) is no window and stays out.
#[test]
fn a_window_nobody_has_seen_yet_arrives_as_its_own_window() {
    let mut headers = vec![
        ("anthropic-ratelimit-unified-slow-status", "active"),
        (
            "anthropic-ratelimit-unified-slow-budget-utilization",
            "0.35",
        ),
        (
            "anthropic-ratelimit-unified-slow-budget-reset",
            "1791401400",
        ),
    ];
    headers.extend_from_slice(FABLE_PROBE);
    let reading = read(&headers).expect("reading");
    assert_eq!(names(&reading), ["slow-budget", "5h", "7d", "7d_oi"]);
    let slow = window(&reading, "slow-budget");
    assert_eq!(slow.utilization, 35.0);
    assert_eq!(slow.resets_at, Some(1_791_401_400));
    assert_eq!(slow.status, None, "it sent no status of its own");
    assert_eq!(
        reading.status,
        Some(LimitStatus::Allowed),
        "the lane's state is not the response's status"
    );
}

/// A window whose name ends in another window's name never answers for it,
/// whichever comes first: `early-5h` (made up) sends a reset and a status, `5h`
/// sends neither.
#[test]
fn a_window_never_takes_the_fields_of_a_name_it_ends_with() {
    let reading = read(&[
        ("anthropic-ratelimit-unified-early-5h-utilization", "0.1"),
        ("anthropic-ratelimit-unified-early-5h-reset", "1791400000"),
        ("anthropic-ratelimit-unified-early-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.2"),
    ])
    .expect("reading");
    let five = window(&reading, "5h");
    assert_eq!(five.resets_at, None);
    assert_eq!(five.status, None);
    assert_eq!(window(&reading, "early-5h").resets_at, Some(1_791_400_000));
}

#[test]
fn every_status_claude_code_knows_reads_and_others_do_not() {
    let status_of = |value: &str| {
        read(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.9"),
            ("anthropic-ratelimit-unified-5h-status", value),
        ])
        .expect("reading")
        .windows[0]
            .status
    };
    assert_eq!(status_of("allowed"), Some(LimitStatus::Allowed));
    assert_eq!(status_of("allowed_warning"), Some(LimitStatus::Warning));
    assert_eq!(status_of("rejected"), Some(LimitStatus::Rejected));
    assert_eq!(status_of("Rejected"), Some(LimitStatus::Rejected));
    assert_eq!(status_of("throttled"), None, "an unknown status is none");
}

/// The walk compares `>=` against whole-number lines, so a two-decimal
/// fraction must land on the exact percentage, not a hair below it.
#[test]
fn a_two_decimal_fraction_reads_as_the_exact_percentage() {
    for (value, pct) in [
        ("0.29", 29.0),
        ("0.57", 57.0),
        ("0.58", 58.0),
        ("1.0", 100.0),
    ] {
        let reading =
            read(&[("anthropic-ratelimit-unified-5h-utilization", value)]).expect("reading");
        assert_eq!(reading.windows[0].utilization, pct, "{value}");
    }
}

#[test]
fn a_response_without_a_window_is_no_reading() {
    assert_eq!(read(&[]), None);
    assert_eq!(
        read(&[("request-id", "req_1"), ("retry-after", "60")]),
        None,
        "headers of other families are no reading"
    );
    assert_eq!(
        read(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "1791397800"),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
        ]),
        None,
        "a status without any window is no reading"
    );
    assert_eq!(
        read(&[("anthropic-ratelimit-unified--utilization", "0.5")]),
        None,
        "a utilization with no window name is none"
    );
}

#[test]
fn header_names_match_in_any_case() {
    let reading = read(&[
        ("Anthropic-RateLimit-Unified-5h-Utilization", "0.5"),
        ("ANTHROPIC-RATELIMIT-UNIFIED-5H-RESET", "1791397800"),
        ("anthropic-ratelimit-unified-5h-status", "Allowed"),
    ])
    .expect("reading");
    let five = window(&reading, "5h");
    assert_eq!(five.utilization, 50.0);
    assert_eq!(five.resets_at, Some(1_791_397_800));
    assert_eq!(five.status, Some(LimitStatus::Allowed));
}

#[test]
fn an_unusable_value_drops_only_its_own_field() {
    let reading = read(&[
        ("anthropic-ratelimit-unified-5h-utilization", "lots"),
        ("anthropic-ratelimit-unified-7d-utilization", " 0.25 "),
        ("anthropic-ratelimit-unified-7d-reset", "next week"),
        ("anthropic-ratelimit-unified-7d-status", "throttled"),
        ("anthropic-ratelimit-unified-7d_oi-utilization", "NaN"),
        ("anthropic-ratelimit-unified-overage-utilization", "-0.5"),
        ("anthropic-ratelimit-unified-grace-5h-utilization", "1e307"),
        ("anthropic-ratelimit-unified-status", "allowed"),
    ])
    .expect("the 7d window still reads");
    assert_eq!(
        names(&reading),
        ["7d"],
        "a window without a finite, non-negative utilization is dropped, the rest stays"
    );
    let week = window(&reading, "7d");
    assert_eq!(week.utilization, 25.0);
    assert_eq!(week.resets_at, None, "an unparseable reset is no reset");
    assert_eq!(week.status, None, "an unknown status is no status");
    assert_eq!(reading.status, Some(LimitStatus::Allowed));
}

/// One window per name, from the first copy, as `HeaderMap::get` reads it,
/// even when the first copy is the unusable one.
#[test]
fn the_first_of_a_repeated_header_wins() {
    let reading = read(&[
        ("anthropic-ratelimit-unified-5h-utilization", "0.1"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.9"),
    ])
    .expect("reading");
    assert_eq!(names(&reading), ["5h"]);
    assert_eq!(reading.windows[0].utilization, 10.0);
    assert_eq!(
        read(&[
            ("anthropic-ratelimit-unified-5h-utilization", ""),
            ("anthropic-ratelimit-unified-5h-utilization", "0.4"),
        ]),
        None,
    );
}

/// A probe's own clock for the answers below: 2026-10-07 16:20:00 UTC, before
/// every reset the fixtures carry.
const NOW: i64 = 1_791_390_000;

fn reading_of(answer: ProbeAnswer) -> UsageInfo {
    match answer {
        ProbeAnswer::Reading(info) => *info,
        other => panic!("no reading: {other:?}"),
    }
}

fn reset_secs(window: Option<&crate::usage::UsageWindow>) -> Option<i64> {
    window
        .and_then(|w| w.resets_at.as_deref())
        .and_then(iso_to_epoch_secs)
}

#[test]
fn a_2xx_with_the_headers_is_the_store_reading() {
    let info = reading_of(answer_from(200, FABLE_PROBE.iter().copied(), NOW));
    let five = info.five_hour.as_ref().expect("5h");
    assert_eq!(five.utilization, 0.0);
    assert_eq!(reset_secs(Some(five)), Some(1_791_397_800));
    let week = info.seven_day.as_ref().expect("7d");
    assert_eq!(week.utilization, 20.0);
    assert_eq!(reset_secs(Some(week)), Some(1_791_648_000));
    assert!(
        info.weekly_scoped.is_empty(),
        "7d_oi names no store window yet, so it stays out of the limits"
    );
    assert!(info.plan.is_none(), "a probe learns no plan");
    assert_eq!(
        info.fetched_at, None,
        "the landing stamps the age, not the parse"
    );
}

#[test]
fn a_spent_window_429_is_a_reading_not_a_throttle() {
    let headers = window_exhaustion_429_headers(1_791_400_000, 1_791_700_000);
    let info = reading_of(answer_from(
        429,
        headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        NOW,
    ));
    assert_eq!(info.five_hour.as_ref().expect("5h").utilization, 100.0);
    assert_eq!(reset_secs(info.five_hour.as_ref()), Some(1_791_400_000));
    assert_eq!(info.seven_day.as_ref().expect("7d").utilization, 62.0);
}

#[test]
fn a_429_without_a_window_is_a_throttle_with_its_retry_after() {
    let retry_after =
        |headers: &[(&str, &str)]| match answer_from(429, headers.iter().copied(), NOW) {
            ProbeAnswer::RateLimited { retry_after } => retry_after,
            other => panic!("not a throttle: {other:?}"),
        };
    assert_eq!(
        retry_after(&[("retry-after", "30")]),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        retry_after(&[("Retry-After", "30")]),
        Some(Duration::from_secs(30)),
        "header names match in any case"
    );
    assert_eq!(
        retry_after(&[("retry-after", "Wed, 07 Oct 2026 16:22:00 GMT")]),
        Some(Duration::from_secs(120)),
        "a date is read against the probe's own clock"
    );
    assert_eq!(retry_after(&[]), None);
    assert_eq!(
        retry_after(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "1791397800"),
        ]),
        None,
        "a status without a window says nothing about the account"
    );
}

#[test]
fn windows_without_5h_or_7d_are_no_reading() {
    let overage = [
        ("anthropic-ratelimit-unified-overage-utilization", "0.4"),
        ("anthropic-ratelimit-unified-overage-reset", "1791397800"),
        ("retry-after", "30"),
    ];
    assert!(
        matches!(
            answer_from(200, overage.iter().copied(), NOW),
            ProbeAnswer::Failed
        ),
        "a 2xx"
    );
    assert!(
        matches!(
            answer_from(429, overage.iter().copied(), NOW),
            ProbeAnswer::RateLimited {
                retry_after: Some(d)
            } if d == Duration::from_secs(30)
        ),
        "a 429"
    );
    let info = reading_of(answer_from(
        200,
        overage[..1]
            .iter()
            .copied()
            .chain([("anthropic-ratelimit-unified-7d-utilization", "0.2")]),
        NOW,
    ));
    assert!(info.five_hour.is_none(), "one of the two is enough");
    assert_eq!(info.seven_day.expect("7d").utilization, 20.0);
}

#[test]
fn a_refused_token_is_rejected_whatever_else_the_response_says() {
    for status in [401, 403] {
        assert!(
            matches!(
                answer_from(status, FABLE_PROBE.iter().copied(), NOW),
                ProbeAnswer::Rejected
            ),
            "{status} with the headers"
        );
        assert!(
            matches!(answer_from(status, [], NOW), ProbeAnswer::Rejected),
            "{status} without them"
        );
    }
}

#[test]
fn any_other_answer_is_a_failure() {
    assert!(
        matches!(
            answer_from(200, [("request-id", "req_1")], NOW),
            ProbeAnswer::Failed
        ),
        "a 2xx without the headers is no reading"
    );
    for status in [300, 400, 404, 500, 529] {
        assert!(
            matches!(
                answer_from(status, FABLE_PROBE.iter().copied(), NOW),
                ProbeAnswer::Failed
            ),
            "{status}"
        );
    }
}

/// The probe sends the kick's request on [`MODEL`]: the kick's header set, off
/// the constants the kick sends, and a 1-token body with the Claude Code system
/// prefix an OAuth token is refused without.
#[test]
fn the_probe_sends_the_kicks_request_on_the_probe_model() {
    let home = HomeSandbox::new();
    let headers: Vec<(String, String)> = FABLE_PROBE
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (base, server) =
        serve_endpoints_raw_with_headers(1, move |_, _| (200, headers.clone(), "{}".into()));
    let _endpoints = EndpointSandbox::new(&home, &base);

    let info = reading_of(probe("sk-ant-oat01-TEST"));
    assert_eq!(info.seven_day.as_ref().expect("7d").utilization, 20.0);

    let raw = server.join().expect("listener");
    assert_eq!(raw.len(), 1, "one probe is one request");
    let req = &raw[0];
    assert!(
        req.starts_with("POST /v1/messages?beta=true "),
        "{:?}",
        req.lines().next()
    );
    for (name, want) in [
        ("content-type", "application/json"),
        ("accept", "application/json"),
        ("authorization", "Bearer sk-ant-oat01-TEST"),
        ("anthropic-version", "2023-06-01"),
        ("anthropic-beta", crate::oauth::KICK_ANTHROPIC_BETA),
        ("anthropic-dangerous-direct-browser-access", "true"),
        ("user-agent", crate::usage::cli_user_agent()),
        ("x-app", "cli"),
        ("x-stainless-lang", "js"),
        ("x-stainless-runtime", "node"),
        (
            "x-stainless-package-version",
            crate::oauth::KICK_STAINLESS_PACKAGE_VERSION,
        ),
    ] {
        assert_eq!(request_header(req, name).as_deref(), Some(want), "{name}");
    }
    let body: serde_json::Value = serde_json::from_str(&request_body(req)).expect("json body");
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["max_tokens"], 1);
    assert_eq!(body["system"][0]["text"], crate::oauth::KICK_SYSTEM_PROMPT);
    assert_eq!(body["messages"][0]["content"], "x");
}

#[test]
fn a_probe_the_server_refuses_reads_rejected() {
    let home = HomeSandbox::new();
    let (base, server) = serve_endpoints_raw_with_headers(1, |_, _| (401, Vec::new(), "{}".into()));
    let _endpoints = EndpointSandbox::new(&home, &base);
    assert!(matches!(probe("sk-ant-oat01-DEAD"), ProbeAnswer::Rejected));
    assert_eq!(server.join().expect("listener").len(), 1);
}

#[test]
fn a_probe_that_reaches_no_server_reads_failed() {
    let home = HomeSandbox::new();
    let closed = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = closed.local_addr().expect("addr").port();
    drop(closed);
    let _endpoints = EndpointSandbox::new(&home, &format!("http://127.0.0.1:{port}"));
    assert!(matches!(probe("sk-ant-oat01-TEST"), ProbeAnswer::Failed));
}
