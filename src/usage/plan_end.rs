//! When an account's paid plan ends or renews: the date the Usage tab,
//! `clauth list` and the feeds show.
//!
//! Neither OAuth endpoint clauth reads (`/api/oauth/usage`,
//! `/api/oauth/profile`) names a billing date. `/profile`'s
//! `subscription_created_at` is no billing anchor: a plan change moves the
//! billing day and keeps that start. A cancellation shows on `/profile` only
//! once the plan has ended. So both dates are the operator's, copied off
//! claude.ai's billing page into the profile's `config.toml`: `plan_ends` for
//! a canceled plan's last day, `plan_renews` for a renewing plan's next
//! billing date.

use chrono::{DateTime, Datelike, Local, Months, NaiveDate};

/// The two dates the operator may give a profile. A marked end wins over a
/// renewal: a canceled plan bills no more.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PlanDates {
    /// `plan_ends`: the last day of a canceled plan.
    pub(crate) ends: Option<NaiveDate>,
    /// `plan_renews`: a billing date of a renewing plan, carried forward a
    /// month at a time once it passes.
    pub(crate) renews: Option<NaiveDate>,
}

impl PlanDates {
    /// When this plan ends or renews, as of the local date `today`: the marked
    /// end (ended once `today` is past it), else the next renewal on or after
    /// `today`. `None` when neither date is set.
    pub(crate) fn end(self, today: NaiveDate) -> Option<PlanEnd> {
        if let Some(day) = self.ends {
            return Some(if day < today {
                PlanEnd::Ended(day)
            } else {
                PlanEnd::Ends(day)
            });
        }
        self.renews
            .and_then(|anchor| next_renewal(anchor, today))
            .map(PlanEnd::Renews)
    }
}

/// When a plan stops serving or renews, each a local calendar day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanEnd {
    /// The canceled plan's last day, today or later.
    Ends(NaiveDate),
    /// The canceled plan's last day, now past.
    Ended(NaiveDate),
    /// The plan's next billing day, today or later.
    Renews(NaiveDate),
}

impl PlanEnd {
    /// The day this end falls on.
    pub(crate) fn day(self) -> NaiveDate {
        match self {
            PlanEnd::Ends(day) | PlanEnd::Ended(day) | PlanEnd::Renews(day) => day,
        }
    }
}

/// The first monthly anniversary of `anchor` that falls on or after `today`,
/// the anchor itself included. Each one is counted from the anchor, not from
/// the one before it, so a 31st bills on the last day of a shorter month and
/// on the 31st again after it. `None` past chrono's range.
pub(crate) fn next_renewal(anchor: NaiveDate, today: NaiveDate) -> Option<NaiveDate> {
    if anchor >= today {
        return Some(anchor);
    }
    // The anniversary in today's own month is the earliest that can fall on
    // or after today: each one stays in its own calendar month, clamped or
    // not, so every earlier one falls in an earlier month.
    let apart = (today.year() - anchor.year()) * 12 + today.month() as i32 - anchor.month() as i32;
    let mut months = u32::try_from(apart).ok()?;
    loop {
        let day = anchor.checked_add_months(Months::new(months))?;
        if day >= today {
            return Some(day);
        }
        months = months.checked_add(1)?;
    }
}

/// The local calendar date of an epoch-seconds instant: the day a plan date is
/// compared against, in the zone the operator typed it in.
pub(crate) fn local_date_of(epoch_secs: i64) -> Option<NaiveDate> {
    Some(
        DateTime::from_timestamp(epoch_secs, 0)?
            .with_timezone(&Local)
            .date_naive(),
    )
}

/// `plan_ends` or `plan_renews` as typed in `config.toml`: a `YYYY-MM-DD`
/// date. Anything else reads as unset, and the load then comments the line
/// out, the visible signal it was not understood (the rule an ignored
/// threshold follows).
pub(crate) fn parse_plan_date(raw: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").ok()
}

/// The canonical `config.toml` spelling of a plan date.
pub(crate) fn render_plan_date(date: NaiveDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

#[cfg(test)]
#[path = "../../tests/inline/plan_end.rs"]
mod tests;
