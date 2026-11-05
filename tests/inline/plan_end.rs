//! The plan end: the operator's `plan_ends` and `plan_renews` days and the
//! monthly carry of a billing day.

use chrono::NaiveDate;

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

/// Each anniversary is counted from the operator's day, so a 31st lands on a
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
    assert_eq!(both.end(today), Some(PlanEnd::Ends(date(2030, 4, 20))));
    let renewing = PlanDates {
        ends: None,
        renews: Some(date(2030, 3, 10)),
    };
    assert_eq!(
        renewing.end(today),
        Some(PlanEnd::Renews(date(2030, 4, 10))),
        "a renewal carries forward to its next billing day"
    );
    assert_eq!(PlanDates::default().end(today), None);
}

#[test]
fn a_marked_end_reads_ended_once_past() {
    let today = date(2026, 11, 4);
    let ends = |day| PlanDates {
        ends: Some(day),
        renews: None,
    };
    assert_eq!(
        ends(today).end(today),
        Some(PlanEnd::Ends(today)),
        "a plan still serves on its last day"
    );
    assert_eq!(
        ends(date(2026, 11, 3)).end(today),
        Some(PlanEnd::Ended(date(2026, 11, 3)))
    );
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
    ] {
        assert_eq!(parse_plan_date(typo), None, "{typo:?} is no date");
    }
    assert_eq!(render_plan_date(date(2026, 11, 5)), "2026-11-05");
    assert_eq!(
        parse_plan_date(&render_plan_date(date(2026, 11, 5))),
        Some(date(2026, 11, 5))
    );
}

#[test]
fn days_left_counts_whole_days_and_none_once_ended() {
    let today = date(2030, 4, 6);
    assert_eq!(PlanEnd::Ends(date(2030, 4, 10)).days_left(today), Some(4));
    assert_eq!(PlanEnd::Renews(today).days_left(today), Some(0));
    assert_eq!(PlanEnd::Ended(date(2030, 4, 5)).days_left(today), None);
}
