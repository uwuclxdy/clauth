//! The plan end: the user's `plan_ends` and `plan_renews` days, the monthly
//! carry of a billing day, and the local day both are compared against.

use chrono::{NaiveDate, TimeZone as _};

use super::{PlanDates, PlanEnd, next_renewal, parse_plan_date, render_plan_date};

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("fixture date is valid")
}

#[test]
fn a_billing_day_still_ahead_is_the_renewal() {
    assert_eq!(
        next_renewal(date(2030, 4, 10), date(2030, 4, 6)),
        Some(date(2030, 4, 10))
    );
    assert_eq!(
        next_renewal(date(2030, 4, 10), date(2030, 4, 10)),
        Some(date(2030, 4, 10)),
        "the billing day itself is still the renewal"
    );
}

#[test]
fn a_past_billing_day_carries_forward_a_month_at_a_time() {
    assert_eq!(
        next_renewal(date(2030, 4, 10), date(2030, 4, 11)),
        Some(date(2030, 5, 10))
    );
    assert_eq!(
        next_renewal(date(2030, 4, 10), date(2031, 1, 20)),
        Some(date(2031, 2, 10)),
        "counting crosses a year end"
    );
    assert_eq!(
        next_renewal(date(2030, 4, 10), date(2030, 5, 10)),
        Some(date(2030, 5, 10)),
        "a carried billing day is still the renewal on the day itself"
    );
}

/// Each anniversary is counted from the user's day, so a 31st lands on a
/// shorter month's last day without drifting there for good.
#[test]
fn a_31st_clamps_to_short_months_and_comes_back() {
    let anchor = date(2026, 1, 31);
    assert_eq!(
        next_renewal(anchor, date(2026, 2, 1)),
        Some(date(2026, 2, 28))
    );
    assert_eq!(
        next_renewal(anchor, date(2026, 3, 1)),
        Some(date(2026, 3, 31)),
        "march bills on the 31st again, not the 28th"
    );
    assert_eq!(
        next_renewal(anchor, date(2026, 4, 1)),
        Some(date(2026, 4, 30))
    );
    assert_eq!(
        next_renewal(date(2027, 12, 31), date(2028, 2, 1)),
        Some(date(2028, 2, 29)),
        "a leap february takes its 29th"
    );
}

#[test]
fn a_marked_end_wins_over_a_renewal() {
    let today = date(2030, 4, 6);
    let both = PlanDates {
        ends: Some(date(2030, 4, 20)),
        renews: Some(date(2030, 4, 10)),
    };
    assert_eq!(
        both.end(today, false),
        Some(PlanEnd::Ends(date(2030, 4, 20)))
    );
    let renewing = PlanDates {
        ends: None,
        renews: Some(date(2030, 3, 10)),
    };
    assert_eq!(
        renewing.end(today, false),
        Some(PlanEnd::Renews(date(2030, 4, 10))),
        "a renewal carries forward to its next billing day"
    );
    assert_eq!(PlanDates::default().end(today, false), None);
}

/// A past end keeps its own day and reads `ended` on a printed surface; the
/// published kind stays `ends`.
#[test]
fn a_marked_end_reads_ended_once_past() {
    let today = date(2026, 11, 4);
    let ends = |day| PlanDates {
        ends: Some(day),
        renews: None,
    };
    let last_day = ends(today).end(today, false);
    assert_eq!(
        (last_day, last_day.map(|e| e.verb(today))),
        (Some(PlanEnd::Ends(today)), Some("ends")),
        "a plan still serves on its last day"
    );
    let past = ends(date(2026, 11, 3)).end(today, false);
    assert_eq!(
        (past, past.map(|e| e.verb(today))),
        (Some(PlanEnd::Ends(date(2026, 11, 3))), Some("ended"))
    );
}

/// A `plan_renews` later than a past `plan_ends` is a plan taken up again:
/// the renewal wins. One on or before that end leaves the plan ended, and a
/// coming end still wins over any renewal.
#[test]
fn a_renewal_later_than_a_past_end_wins() {
    let today = date(2026, 11, 14);
    let dates = |ends, renews| PlanDates {
        ends: Some(ends),
        renews: Some(renews),
    };
    assert_eq!(
        dates(date(2026, 9, 2), date(2026, 10, 20)).end(today, false),
        Some(PlanEnd::Renews(date(2026, 11, 20))),
        "a later billing day carries forward from its own anchor"
    );
    assert_eq!(
        dates(date(2026, 9, 2), date(2026, 9, 2)).end(today, false),
        Some(PlanEnd::Ends(date(2026, 9, 2))),
        "a billing day on the end day itself is no new plan"
    );
    assert_eq!(
        dates(date(2026, 9, 2), date(2026, 3, 15)).end(today, false),
        Some(PlanEnd::Ends(date(2026, 9, 2))),
        "an earlier billing day is the plan that ended"
    );
    assert_eq!(
        dates(date(2026, 12, 1), date(2027, 1, 5)).end(today, false),
        Some(PlanEnd::Ends(date(2026, 12, 1))),
        "a coming end wins over a later renewal"
    );
}

/// A plan whose reading refuses service (canceled, or refused by its
/// organization) renews nothing; its marked end still shows, and a past one
/// stays ended instead of yielding to a later renewal.
#[test]
fn a_plan_that_cannot_serve_shows_no_renewal() {
    let today = date(2026, 11, 14);
    let renewing = PlanDates {
        ends: None,
        renews: Some(date(2026, 12, 15)),
    };
    assert_eq!(renewing.end(today, true), None);
    let ending = PlanDates {
        ends: Some(date(2026, 11, 20)),
        renews: Some(date(2026, 12, 15)),
    };
    assert_eq!(
        ending.end(today, true),
        Some(PlanEnd::Ends(date(2026, 11, 20)))
    );
    let resubscribed = PlanDates {
        ends: Some(date(2026, 9, 2)),
        renews: Some(date(2026, 12, 15)),
    };
    assert_eq!(
        resubscribed.end(today, true),
        Some(PlanEnd::Ends(date(2026, 9, 2)))
    );
}

/// The day a plan date is compared against is the LOCAL calendar day: at
/// every hour of one UTC day, `local_day` names the day chrono's own `Local`
/// clock names. In a zone off UTC some of those hours fall on another local
/// day, so a UTC pick reds here; on a UTC host the two agree by construction,
/// the residual this cannot see.
#[test]
fn the_local_day_is_the_local_zones_calendar_day() {
    use chrono::TimeZone;

    // 2026-11-14T00:00:00Z.
    let midnight_utc = 1_794_614_400_i64;
    for hour in 0..24 {
        let at = midnight_utc + hour * 3600;
        assert_eq!(
            crate::format::local_day(at),
            chrono::Local
                .timestamp_opt(at, 0)
                .single()
                .map(|t| t.date_naive()),
            "hour {hour} UTC"
        );
    }
}

#[test]
fn a_plan_date_reads_an_iso_date_and_nothing_else() {
    assert_eq!(parse_plan_date("2026-11-20"), Some(date(2026, 11, 20)));
    assert_eq!(parse_plan_date(" 2026-11-20 "), Some(date(2026, 11, 20)));
    for typo in [
        "",
        "2026-13-01",
        "2026-02-30",
        "20/11/2026",
        "next week",
        "2026-11-20T00:00:00Z",
        "26-12-15",
        "202-01-05",
        "+2026-01-05",
        "02026-01-05",
    ] {
        assert_eq!(parse_plan_date(typo), None, "{typo:?} is no date");
    }
    assert_eq!(render_plan_date(date(2026, 11, 5)), "2026-11-05");
    assert_eq!(
        parse_plan_date(&render_plan_date(date(2026, 11, 5))),
        Some(date(2026, 11, 5))
    );
}

/// The epoch second `hour`:00 local falls on `y-m-d`.
fn local_at(y: i32, m: u32, d: u32, hour: u32) -> i64 {
    chrono::Local
        .with_ymd_and_hms(y, m, d, hour, 0, 0)
        .single()
        .expect("an hour no clock change skips or repeats")
        .timestamp()
}

/// Whole calendar days from today to the plan day: `0` on the day itself,
/// across a month end, and negative once the day has passed.
#[test]
fn days_left_counts_whole_days_and_zero_on_the_day() {
    let today = date(2030, 4, 6);
    assert_eq!(
        [
            PlanEnd::Ends(date(2030, 4, 10)).days_left(today),
            PlanEnd::Renews(today).days_left(today),
            PlanEnd::Renews(date(2030, 5, 1)).days_left(today),
            PlanEnd::Ends(date(2030, 4, 5)).days_left(today),
        ],
        [4, 0, 25, -1],
        "(4 days, the day itself, across april's end, a day past)"
    );
}

/// A plan day brought forward is today or later and starts before the week's
/// reset: a renewal three days out against a reset five days out or at
/// 18:00 on the renewal day itself, a plan ending today. Nothing comes
/// forward for a day starting at or after the reset, a week already over at
/// `now`, an end already past or no date at all.
#[test]
fn end_before_reset_brings_forward_a_day_starting_before_the_reset() {
    let now = local_at(2030, 4, 6, 12);
    let renews = PlanDates {
        ends: None,
        renews: Some(date(2030, 1, 9)),
    };
    let ends = |day| PlanDates {
        ends: Some(day),
        renews: None,
    };
    assert_eq!(
        [
            renews.end_before_reset(now, local_at(2030, 4, 11, 18)),
            renews.end_before_reset(now, local_at(2030, 4, 9, 18)),
            ends(date(2030, 4, 6)).end_before_reset(now, local_at(2030, 4, 11, 18)),
        ],
        [
            Some(PlanEnd::Renews(date(2030, 4, 9))),
            Some(PlanEnd::Renews(date(2030, 4, 9))),
            Some(PlanEnd::Ends(date(2030, 4, 6))),
        ],
        "(a reset 5 days out, a reset later on the day itself, ending today)"
    );
    assert_eq!(
        [
            renews.end_before_reset(now, local_at(2030, 4, 9, 0)),
            renews.end_before_reset(now, local_at(2030, 4, 7, 18)),
            renews.end_before_reset(now, now),
            renews.end_before_reset(now, now - 3600),
            ends(date(2030, 4, 5)).end_before_reset(now, local_at(2030, 4, 11, 18)),
            PlanDates::default().end_before_reset(now, local_at(2030, 4, 11, 18)),
        ],
        [None; 6],
        "(a reset at the day's midnight, a reset before the day, a reset at now, \
         a reset past, an end past, no date)"
    );
}

/// The plan day's start has one definition, `crate::format::local_day_start`,
/// which the soonest-weekly-reset walk's plan cap and the plan chip's
/// comparison both call.
#[test]
fn local_day_start_has_one_definition_the_walk_cap_and_the_plan_chip_share() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let scanned = crate::testutil::rs_files(&src);
    assert!(scanned.len() > 100, "the walk read the crate: {scanned:?}");
    let read = |path: &std::path::Path| std::fs::read_to_string(path).expect("a source file");
    let defining: Vec<_> = scanned
        .iter()
        .filter(|path| read(path).contains("fn local_day_start("))
        .cloned()
        .collect();
    assert_eq!(defining, [src.join("format.rs")], "the one definition");
    // Doc comments stripped: a `///` link names the same path the call does,
    // and a pin that matches either cannot tell the call from prose about it.
    let code = |path: &std::path::Path| -> String {
        read(path)
            .lines()
            .filter(|line| !line.trim_start().starts_with("///"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    for caller in ["fallback.rs", "usage/plan_end.rs"] {
        assert!(
            code(&src.join(caller)).contains("crate::format::local_day_start"),
            "{caller} calls the shared definition"
        );
    }
}
