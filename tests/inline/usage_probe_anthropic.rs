use super::reading_from_headers;
use crate::usage::probe::{ProbeReading, ProbeStatus, ProbeWindow};

fn read(pairs: &[(&str, &str)]) -> Option<ProbeReading> {
    reading_from_headers(pairs.iter().copied())
}

fn names(reading: &ProbeReading) -> Vec<&str> {
    reading.windows.iter().map(|w| w.name.as_str()).collect()
}

fn window<'r>(reading: &'r ProbeReading, name: &str) -> &'r ProbeWindow {
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
        &ProbeWindow {
            name: "5h".into(),
            utilization: 0.0,
            resets_at: Some(1_791_397_800),
            status: Some(ProbeStatus::Allowed),
        }
    );
    let week = window(&reading, "7d");
    assert_eq!(week.utilization, 20.0);
    assert_eq!(week.resets_at, Some(1_791_648_000));
    assert_eq!(window(&reading, "7d_oi").resets_at, Some(1_791_648_000));
    assert_eq!(reading.status, Some(ProbeStatus::Allowed));
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
    assert_eq!(five.status, Some(ProbeStatus::Rejected));
    let week = window(&reading, "7d");
    assert_eq!(week.utilization, 62.0, "0.62 reads as exactly 62");
    assert_eq!(week.resets_at, Some(1_791_700_000));
    assert_eq!(week.status, Some(ProbeStatus::Allowed));
    assert_eq!(reading.status, Some(ProbeStatus::Rejected));
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
        Some(ProbeStatus::Allowed),
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
    assert_eq!(status_of("allowed"), Some(ProbeStatus::Allowed));
    assert_eq!(status_of("allowed_warning"), Some(ProbeStatus::Warning));
    assert_eq!(status_of("rejected"), Some(ProbeStatus::Rejected));
    assert_eq!(status_of("Rejected"), Some(ProbeStatus::Rejected));
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
    assert_eq!(five.status, Some(ProbeStatus::Allowed));
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
    assert_eq!(reading.status, Some(ProbeStatus::Allowed));
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
