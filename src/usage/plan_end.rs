//! When an account's paid plan ends or renews: the date the Usage tab,
//! `clauth list` and the feeds show.
//!
//! No endpoint clauth reads names a billing date: the Claude Code 2.1.319
//! binary carries no billing-day field (measured 2026-11-06). So both dates
//! are the user's, copied off claude.ai's billing page into the profile's
//! `config.toml`: `plan_ends` for a canceled plan's last day, `plan_renews`
//! for a renewing plan's billing day. A billing day rolls forward a month at a
//! time, a guess the plan's `/profile` status overrules: a plan that reads
//! canceled or refused shows no renewal.

use chrono::{Datelike, Months, NaiveDate};

/// The two dates the user may give a profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PlanDates {
    /// `plan_ends`: the last day of a canceled plan.
    pub(crate) ends: Option<NaiveDate>,
    /// `plan_renews`: a billing date of a renewing plan, carried forward a
    /// month at a time once it passes.
    pub(crate) renews: Option<NaiveDate>,
}

impl PlanDates {
    /// When this plan ends or renews, as of the local date `today`. A marked
    /// end still to come wins over a renewal: a canceled plan bills no more.
    /// A past one stands unless `plan_renews` is later than it, the plan
    /// taken up again. A plan whose reading `refuses_service` (canceled, or
    /// refused by its organization) renews nothing. `None` when no date
    /// applies.
    pub(crate) fn end(self, today: NaiveDate, refuses_service: bool) -> Option<PlanEnd> {
        let renews = self.renews.filter(|_| !refuses_service);
        match self.ends {
            Some(day) if day >= today || renews.is_none_or(|anchor| anchor <= day) => {
                Some(PlanEnd::Ends(day))
            }
            _ => renews
                .and_then(|anchor| next_renewal(anchor, today))
                .map(PlanEnd::Renews),
        }
    }
}

/// When a plan stops serving or renews, each a local calendar day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanEnd {
    /// The canceled plan's last day, kept as it is once it passes.
    Ends(NaiveDate),
    /// The plan's next billing day, today or later.
    Renews(NaiveDate),
}

impl PlanEnd {
    /// The day this end falls on.
    pub(crate) fn day(self) -> NaiveDate {
        match self {
            Self::Ends(day) | Self::Renews(day) => day,
        }
    }

    /// The word a printed surface names this end by on `today`: an end whose
    /// day has passed reads `ended`. The published kind stays `ends`.
    pub(crate) fn verb(self, today: NaiveDate) -> &'static str {
        match self {
            Self::Ends(day) if day < today => "ended",
            Self::Ends(_) => "ends",
            Self::Renews(_) => "renews",
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

/// `plan_ends` or `plan_renews` as typed in `config.toml`: a `YYYY-MM-DD`
/// date with a four-digit year. Anything else reads as unset, and the load
/// then comments the line out, the visible signal it was not understood (the
/// rule an ignored threshold follows).
pub(crate) fn parse_plan_date(raw: &str) -> Option<NaiveDate> {
    let raw = raw.trim();
    // `%Y` alone takes one to four digits, so `26-12-15` would read as the
    // year 26 and be written back as `0026-12-15`.
    let (year, _) = raw.split_once('-')?;
    if year.len() != 4 || !year.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()
}

/// The canonical `config.toml` spelling of a plan date.
pub(crate) fn render_plan_date(date: NaiveDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

#[cfg(test)]
#[path = "../../tests/inline/plan_end.rs"]
mod tests;
