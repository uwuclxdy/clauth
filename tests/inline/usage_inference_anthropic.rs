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
        ProbeAnswer::Reading { info, .. } => *info,
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
    let info = reading_of(answer_from(200, None, FABLE_PROBE.iter().copied(), NOW));
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
        None,
        headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        NOW,
    ));
    assert_eq!(info.five_hour.as_ref().expect("5h").utilization, 100.0);
    assert_eq!(reset_secs(info.five_hour.as_ref()), Some(1_791_400_000));
    assert_eq!(info.seven_day.as_ref().expect("7d").utilization, 62.0);
}

/// A spent window says itself why the limiter refuses, so its 429 carries no
/// limiter verdict: the account reads `5h 100%`, not `claude code blocked`.
#[test]
fn a_spent_window_429_carries_no_limiter_verdict() {
    let headers = window_exhaustion_429_headers(NOW + 600, 1_791_700_000);
    let ProbeAnswer::Reading { limited, .. } = answer_from(
        429,
        None,
        headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        NOW,
    ) else {
        panic!("no reading");
    };
    assert_eq!(limited, None);
}

/// A reading keeps what the rest of the 429 says: the server's wait, and the
/// limiter's verdict the kick reads off its own 429. A 2xx carries neither.
#[test]
fn a_429_reading_keeps_its_retry_after_and_the_limiters_verdict() {
    // The recorded 429 with its 5h window read at 40%: below the cap, so the
    // reading does not show the refusal.
    let headers: Vec<(String, String)> = window_exhaustion_429_headers(NOW + 600, 1_791_700_000)
        .into_iter()
        .map(|(k, v)| {
            if k == "anthropic-ratelimit-unified-5h-utilization" {
                (k, "0.4".to_string())
            } else {
                (k, v)
            }
        })
        .collect();
    let ProbeAnswer::Reading {
        retry_after,
        limited,
        ..
    } = answer_from(
        429,
        None,
        headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        NOW,
    )
    else {
        panic!("no reading");
    };
    assert_eq!(
        retry_after,
        Some(Duration::from_secs(900)),
        "6072 s, capped at the 15 min every wait is"
    );
    let limited = limited.expect("the 429's verdict");
    assert!(limited.rejected, "`unified-status: rejected`");
    assert_eq!(
        limited.until_epoch_secs,
        Some(NOW + 900),
        "the later of the reset and the wait"
    );

    let ProbeAnswer::Reading {
        retry_after,
        limited,
        ..
    } = answer_from(200, None, FABLE_PROBE.iter().copied(), NOW)
    else {
        panic!("no reading");
    };
    assert_eq!((retry_after, limited), (None, None));
}

#[test]
fn a_429_without_a_window_is_a_throttle_with_its_retry_after() {
    let retry_after =
        |headers: &[(&str, &str)]| match answer_from(429, None, headers.iter().copied(), NOW) {
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
            answer_from(200, None, overage.iter().copied(), NOW),
            ProbeAnswer::Failed
        ),
        "a 2xx"
    );
    assert!(
        matches!(
            answer_from(429, None,overage.iter().copied(), NOW),
            ProbeAnswer::RateLimited {
                retry_after: Some(d)
            } if d == Duration::from_secs(30)
        ),
        "a 429"
    );
    let info = reading_of(answer_from(
        200,
        None,
        overage[..1]
            .iter()
            .copied()
            .chain([("anthropic-ratelimit-unified-7d-utilization", "0.2")]),
        NOW,
    ));
    assert!(info.five_hour.is_none(), "one of the two is enough");
    assert_eq!(info.seven_day.expect("7d").utilization, 20.0);
}

/// A 401 or a 403 whose Anthropic error body refuses the credential is a
/// refused token whatever else the response says.
#[test]
fn a_refused_token_is_rejected_whatever_else_the_response_says() {
    for (status, forbidden) in [
        (401, Some(super::Forbidden::Token)),
        (403, Some(super::Forbidden::Token)),
    ] {
        assert!(
            matches!(
                answer_from(status, forbidden, FABLE_PROBE.iter().copied(), NOW),
                ProbeAnswer::Rejected
            ),
            "{status} with the headers"
        );
        assert!(
            matches!(
                answer_from(status, forbidden, [], NOW),
                ProbeAnswer::Rejected
            ),
            "{status} without them"
        );
    }
}

/// A 403 whose body is no Anthropic refusal says nothing about the token: a
/// page from a proxy, a WAF or a region block in front of the API fails the
/// probe, headers or not.
#[test]
fn a_403_without_an_anthropic_refusal_is_a_failure() {
    for forbidden in [None, Some(super::Forbidden::Unrecognized)] {
        assert!(
            matches!(
                answer_from(403, forbidden, FABLE_PROBE.iter().copied(), NOW),
                ProbeAnswer::Failed
            ),
            "{forbidden:?}"
        );
    }
}

#[test]
fn any_other_answer_is_a_failure() {
    assert!(
        matches!(
            answer_from(200, None, [("request-id", "req_1")], NOW),
            ProbeAnswer::Failed
        ),
        "a 2xx without the headers is no reading"
    );
    for status in [300, 400, 404, 500, 529] {
        assert!(
            matches!(
                answer_from(status, None, FABLE_PROBE.iter().copied(), NOW),
                ProbeAnswer::Failed
            ),
            "{status}"
        );
    }
}

/// The probe sends the one messages request: Claude Code's header set, off the
/// constants the kick sends, and a 1-token body on Haiku 4.5 with the Claude
/// Code system prefix an OAuth token is refused without. The listener takes a
/// second request, so a probe that sent twice would show.
#[test]
fn the_probe_sends_the_one_messages_request() {
    let home = HomeSandbox::new();
    let headers: Vec<(String, String)> = FABLE_PROBE
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (base, server) =
        serve_endpoints_raw_with_headers(2, move |_, _| (200, headers.clone(), "{}".into()));
    let _endpoints = EndpointSandbox::new(&home, &base);

    let info = reading_of(probe("sk-ant-oat01-TEST", || true).expect("the probe sent"));
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
    assert_eq!(body["model"], "claude-haiku-4-5-20251001");
    assert_eq!(MODEL, "claude-haiku-4-5-20251001");
    assert_eq!(body["max_tokens"], 1);
    assert_eq!(body["system"][0]["text"], crate::oauth::KICK_SYSTEM_PROMPT);
    assert_eq!(body["messages"][0]["content"], "x");
}

/// The Anthropic error body a 403 refusing the credential carries: a
/// `permission_error` with one of the credential codes Claude Code reads.
const TOKEN_REFUSED_403: &str = r#"{"type":"error","error":{"type":"permission_error","message":"OAuth token has been revoked","details":{"error_code":"token_revoked"}}}"#;

/// A Cloudflare block page, the shape a WAF or a region block in front of the
/// API answers with.
const BLOCK_PAGE_403: &str = "<!DOCTYPE html><html><head><title>Attention Required! | Cloudflare</title></head><body><h1>Sorry, you have been blocked</h1></body></html>";

#[test]
fn a_probe_the_server_refuses_reads_rejected() {
    let home = HomeSandbox::new();
    let (base, server) = serve_endpoints_raw_with_headers(2, |_, i| {
        if i == 0 {
            (
                401,
                Vec::new(),
                r#"{"type":"error","error":{"type":"authentication_error"}}"#.into(),
            )
        } else {
            (403, Vec::new(), TOKEN_REFUSED_403.into())
        }
    });
    let _endpoints = EndpointSandbox::new(&home, &base);
    assert!(
        matches!(
            probe("sk-ant-oat01-DEAD", || true),
            Some(ProbeAnswer::Rejected)
        ),
        "a 401 whose Anthropic error body refuses the credential refuses the token"
    );
    crate::usage::reset_request_slots();
    assert!(
        matches!(
            probe("sk-ant-oat01-DEAD", || true),
            Some(ProbeAnswer::Rejected)
        ),
        "a 403 whose Anthropic error body refuses the credential refuses the token"
    );
    assert_eq!(server.join().expect("listener").len(), 2);
}

/// A 403 that is no Anthropic refusal says nothing about the token: a block
/// page from in front of the API, an empty body, or JSON of another shape all
/// fail the probe rather than kill the token.
#[test]
fn a_403_that_is_no_anthropic_refusal_fails_the_probe() {
    let bodies = [
        BLOCK_PAGE_403,
        "",
        r#"{"error":{"type":"permission_error"}}"#,
        r#"{"type":"error","error":{"type":"overloaded_error"}}"#,
    ];
    let home = HomeSandbox::new();
    // One request above the probes sent, so a probe sent twice on one 403 is
    // answered and counted rather than refused at the connect, which would
    // read `Failed` too.
    let (base, server) = serve_endpoints_raw_with_headers(bodies.len() + 1, move |_, i| {
        (
            403,
            Vec::new(),
            bodies.get(i).copied().unwrap_or_default().to_string(),
        )
    });
    let _endpoints = EndpointSandbox::new(&home, &base);
    for body in bodies {
        crate::usage::reset_request_slots();
        let answer = probe("sk-ant-oat01-TEST", || true).expect("the probe sent");
        assert!(matches!(answer, ProbeAnswer::Failed), "{body}: {answer:?}");
    }
    assert_eq!(server.join().expect("listener").len(), bodies.len());
}

/// A 403 that names the organization, not the token, is no refused token: a
/// new token cannot fix it, so the probe must not send the operator to mint
/// one. The body is in the shape Claude Code reads its error codes from.
#[test]
fn a_403_naming_the_organization_is_no_refused_token() {
    let home = HomeSandbox::new();
    let body = r#"{"type":"error","error":{"type":"permission_error","message":"OAuth authentication is currently not allowed for this organization.","details":{"error_code":"oauth_not_allowed_for_organization"}}}"#;
    let (base, server) =
        serve_endpoints_raw_with_headers(2, move |_, _| (403, Vec::new(), body.to_string()));
    let _endpoints = EndpointSandbox::new(&home, &base);
    let answer = probe("sk-ant-oat01-TEST", || true).expect("the probe sent");
    assert_eq!(server.join().expect("listener").len(), 1);
    assert!(
        matches!(answer, ProbeAnswer::OrgRefused { canceled: true }),
        "{answer:?}"
    );
}

/// An organization on hold answers with its own code: the organization refuses
/// the account, which is no canceled plan, and no refused token either.
#[test]
fn a_403_naming_an_organization_on_hold_is_no_canceled_plan() {
    let home = HomeSandbox::new();
    let body = r#"{"type":"error","error":{"type":"permission_error","details":{"error_code":"organization_on_hold"}}}"#;
    let (base, server) =
        serve_endpoints_raw_with_headers(2, move |_, _| (403, Vec::new(), body.to_string()));
    let _endpoints = EndpointSandbox::new(&home, &base);
    let answer = probe("sk-ant-oat01-TEST", || true).expect("the probe sent");
    assert_eq!(server.join().expect("listener").len(), 1);
    assert!(
        matches!(answer, ProbeAnswer::OrgRefused { canceled: false }),
        "{answer:?}"
    );
}

/// A 401 whose body is no Anthropic refusal says nothing about the token: a
/// TLS-intercepting proxy's HTML page with no `request-id` fails the probe
/// rather than kill the token across every restart, and one the server sends
/// still refuses it.
#[test]
fn a_401_that_is_no_anthropic_refusal_fails_the_probe() {
    let bodies = [
        "<html><body>Proxy authentication required</body></html>",
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
    ];
    let home = HomeSandbox::new();
    let (base, server) = serve_endpoints_raw_with_headers(bodies.len() + 1, move |_, i| {
        (
            401,
            Vec::new(),
            bodies.get(i).copied().unwrap_or_default().to_string(),
        )
    });
    let _endpoints = EndpointSandbox::new(&home, &base);
    let proxy = probe("sk-ant-oat01-TEST", || true).expect("the probe sent");
    crate::usage::reset_request_slots();
    let server_refusal = probe("sk-ant-oat01-TEST", || true).expect("the probe sent");
    assert_eq!(server.join().expect("listener").len(), bodies.len());
    assert!(matches!(proxy, ProbeAnswer::Failed), "{proxy:?}");
    assert!(
        matches!(server_refusal, ProbeAnswer::Rejected),
        "{server_refusal:?}"
    );
}

/// A `permission_error` refuses the token with a credential code beside it:
/// without one, or the message saying the OAuth token was revoked, it may
/// refuse anything else about the request, so the probe fails rather than kill
/// the token.
#[test]
fn a_permission_error_refuses_the_token_only_with_a_credential_code() {
    let bare = r#"{"type":"error","error":{"type":"permission_error","message":"not allowed"}}"#;
    assert_eq!(super::forbidden_by(bare), super::Forbidden::Unrecognized);
    for code in [
        "token_expired",
        "token_revoked",
        "token_invalid",
        "credential_invalid",
    ] {
        let body = format!(
            r#"{{"type":"error","error":{{"type":"permission_error","details":{{"error_code":"{code}"}}}}}}"#
        );
        assert_eq!(
            super::forbidden_by(&body),
            super::Forbidden::Token,
            "{code}"
        );
    }
}

/// Claude Code reads a `permission_error` whose message says the OAuth token
/// was revoked as a revoked token on its own, with no credential code beside
/// it, so the token is refused. The control: another message and no code
/// refuses nothing.
#[test]
fn a_permission_error_saying_the_oauth_token_was_revoked_refuses_the_token() {
    let revoked = r#"{"type":"error","error":{"type":"permission_error","message":"OAuth token has been revoked. Please run /login."}}"#;
    assert_eq!(super::forbidden_by(revoked), super::Forbidden::Token);
    let other = r#"{"type":"error","error":{"type":"permission_error","message":"OAuth is not enabled for this route."}}"#;
    assert_eq!(super::forbidden_by(other), super::Forbidden::Unrecognized);
}

/// Only a 403 is read for the organization's code: a 401 is the token's own
/// refusal whatever its body names, so the token is refused.
#[test]
fn only_a_403_is_read_for_the_organization_code() {
    let home = HomeSandbox::new();
    let body = r#"{"type":"error","error":{"type":"authentication_error","details":{"error_code":"oauth_not_allowed_for_organization"}}}"#;
    let (base, server) =
        serve_endpoints_raw_with_headers(2, move |_, _| (401, Vec::new(), body.to_string()));
    let _endpoints = EndpointSandbox::new(&home, &base);
    let answer = probe("sk-ant-oat01-TEST", || true).expect("the probe sent");
    assert_eq!(server.join().expect("listener").len(), 1);
    assert!(matches!(answer, ProbeAnswer::Rejected), "{answer:?}");
}

/// Every code Claude Code reads as the organization's refusal is one here: a
/// new token fixes none of them, so none may send the operator to re-mint.
/// Only the code a canceled subscription answers with reads as canceled.
#[test]
fn every_organization_code_claude_code_reads_is_no_refused_token() {
    for code in [
        "oauth_not_allowed_for_organization",
        "claude_code_key_creator_not_member",
        "restricted_regime_endpoint",
        "organization_on_hold",
        "organization_disabled",
        "ip_not_in_allowed_range",
    ] {
        let body = format!(
            r#"{{"type":"error","error":{{"type":"permission_error","details":{{"error_code":"{code}"}}}}}}"#
        );
        assert_eq!(
            super::forbidden_by(&body),
            super::Forbidden::Organization {
                canceled: code == "oauth_not_allowed_for_organization"
            },
            "{code}"
        );
    }
}

/// The code is read where Claude Code reads one, the first that is there of
/// `error.details.error_code`, `error.error_code` and `error.code`.
#[test]
fn the_organization_code_is_read_where_claude_code_reads_it() {
    for body in [
        r#"{"error":{"details":{"error_code":"oauth_not_allowed_for_organization"}}}"#,
        r#"{"error":{"error_code":"oauth_not_allowed_for_organization"}}"#,
        r#"{"error":{"code":"oauth_not_allowed_for_organization"}}"#,
    ] {
        assert_eq!(
            super::forbidden_by(body),
            super::Forbidden::Organization { canceled: true },
            "{body}"
        );
    }
    for body in [
        r#"{"error":{"details":{"error_code":"token_revoked"},"code":"oauth_not_allowed_for_organization"}}"#,
        r#"{"error":{"type":"permission_error"}}"#,
        "{}",
        "not json",
    ] {
        assert!(
            !matches!(
                super::forbidden_by(body),
                super::Forbidden::Organization { .. }
            ),
            "{body}"
        );
    }
}

/// `on_turn` runs once per request, so the leg flips its row once.
#[test]
fn the_probe_calls_on_turn_once() {
    let home = HomeSandbox::new();
    let (base, server) = serve_endpoints_raw_with_headers(2, |_, _| (200, Vec::new(), "{}".into()));
    let _endpoints = EndpointSandbox::new(&home, &base);
    let turned = std::cell::Cell::new(0);
    let _ = probe("sk-ant-oat01-TEST", || {
        turned.set(turned.get() + 1);
        true
    });
    assert_eq!(turned.get(), 1);
    assert_eq!(server.join().expect("listener").len(), 1);
}

/// `on_turn` comes once the request holds its spacing slot, never during the
/// wait for it, so the leg shows the probe in flight only once it is. The
/// sandbox clears the slots, so a reservation seen from inside the turn is the
/// probe's own.
#[test]
fn the_probe_turns_once_it_holds_its_slot() {
    let home = HomeSandbox::new();
    let (base, server) = serve_endpoints_raw_with_headers(2, |_, _| (200, Vec::new(), "{}".into()));
    let _endpoints = EndpointSandbox::new(&home, &base);
    let held = std::cell::Cell::new(None);
    let _ = probe("sk-ant-oat01-TEST", || {
        held.set(crate::usage::reserved_request_slot(
            crate::usage::ANTHROPIC_ORIGIN,
        ));
        true
    });
    assert_eq!(server.join().expect("listener").len(), 1);
    assert!(held.get().is_some(), "the turn came before the slot");
}

#[test]
fn a_header_is_read_in_any_case_and_the_first_wins() {
    let response = super::MessagesResponse {
        status: 429,
        headers: vec![
            ("Retry-After".to_string(), "30".to_string()),
            ("retry-after".to_string(), "60".to_string()),
        ],
        forbidden: None,
    };
    assert_eq!(response.header("retry-after"), Some("30"));
    assert_eq!(response.header("RETRY-AFTER"), Some("30"));
    assert_eq!(response.header("x-app"), None);
}

#[test]
fn a_probe_that_reaches_no_server_reads_failed() {
    let home = HomeSandbox::new();
    let closed = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = closed.local_addr().expect("addr").port();
    drop(closed);
    let _endpoints = EndpointSandbox::new(&home, &format!("http://127.0.0.1:{port}"));
    assert!(matches!(
        probe("sk-ant-oat01-TEST", || true),
        Some(ProbeAnswer::Failed)
    ));
}
