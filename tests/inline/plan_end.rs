//! The plan end: the operator's `plan_ends` and `plan_renews` days, the
//! monthly carry of a billing day, and the `soonest-plan-end` ranking key.

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

/// Every ending plan ranks ahead of every renewing one, whatever the days:
/// what an ending plan leaves unspent is lost, a renewing plan's week is not.
#[test]
fn an_ending_plan_ranks_ahead_of_a_sooner_renewal() {
    let ends_late = PlanEnd::Ends(date(2027, 3, 1)).rank_key();
    let ends_soon = PlanEnd::Ends(date(2026, 11, 20)).rank_key();
    let renews_soon = PlanEnd::Renews(date(2026, 11, 5)).rank_key();
    let renews_late = PlanEnd::Renews(date(2026, 11, 30)).rank_key();
    assert!(ends_soon < ends_late);
    assert!(ends_late < renews_soon);
    assert!(renews_soon < renews_late);
    assert_eq!(PlanEnd::Ended(date(2026, 11, 3)).rank_key(), None);
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
