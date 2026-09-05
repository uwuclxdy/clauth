//! Durable per-day token ledger for the Tokens tab.
//!
//! # Why
//!
//! The tab's base is CC's `stats-cache.json`, authoritative only up to its
//! `lastComputedDate`; the [`crate::tokens`] top-up bridges the gap by reading
//! `~/.claude/projects/` transcripts strictly newer than that date. That bridge
//! is load-bearing precisely because the base can stay frozen for weeks — but CC
//! also prunes transcripts past `cleanupPeriodDays`. Once a day sits BOTH after a
//! frozen `lastComputedDate` and before the retention horizon, it lives in
//! neither source and is counted nowhere (the "shows too little" report).
//!
//! This ledger closes that hole: the first time a finalized (past) day is seen in
//! the transcripts, its per-model split is written to
//! `~/.clauth/token_ledger.json`, so the day's tokens survive the transcripts
//! being pruned. It doubles as a cold-start bound — the sweep's effective cutoff
//! advances to [`Ledger::recorded_through`], so a fresh process re-reads only days
//! after it instead of everything after a possibly-months-stale base date.
//!
//! # Boundaries
//!
//! - The ledger only ever records days strictly before "today" (a running day is
//!   incomplete). Recording inserts the first-seen split for each finalized
//!   day; the monotonic watermark makes each day record exactly once, so a
//!   later re-read can never lower or double-count a stored day.
//! - [`Ledger::apply_to_base`] folds only days strictly after the base's
//!   `lastComputedDate`, so if CC's own aggregation later catches up past a ledger
//!   day, the ledger never double-counts against the base.
//!
//! # Schema
//!
//! v2 adds per-hour buckets ([`WireModel::hours`]), optional on the wire: a v1
//! file (no `hours` key) loads with `None` and keeps `None` on save. A one-shot
//! backfill ([`Ledger::backfill_hours`], driven from the tokens worker) fills
//! those buckets from the transcript corpus where the stored flat totals still
//! match exactly; the `backfill_done` flag (absent → `false` in pre-backfill
//! files) makes that sweep run at most once per ledger. Days the corpus no
//! longer fully covers keep their v1 shape forever and price at the default
//! tier.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pricing::HourTokens;
use crate::tokens::{DayModelTokens, DayTokens, ModelDayAcc, ModelTokens, TokenStats};
use crate::usage::{epoch_secs_to_iso, iso_to_epoch_secs};

const LEDGER_FILE: &str = "token_ledger.json";

/// One model's stored split for one day (mirrors [`ModelTokens`] without the
/// redundant `model` name, which is the map key). `hours` is the schema-v2
/// hourly axis.
#[derive(Serialize, Deserialize, Default)]
struct WireModel {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    /// Per-hour buckets, index = hour 0..23. A v1 file (no `hours` key) loads
    /// with `None` — serde leaves an absent `Option` field `None`; the explicit
    /// `default` is belt-and-braces per the schema contract. `skip_serializing_if`
    /// keeps a v1 day's wire shape byte-for-byte v1 on save, so the file only
    /// gains `hours` entries as new days are recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hours: Option<[WireHour; 24]>,
}

/// One hour's token buckets on the wire — the serde twin of [`HourTokens`]
/// (which deliberately carries no serde derives).
#[derive(Serialize, Deserialize, Clone, Copy)]
struct WireHour {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
}

impl From<HourTokens> for WireHour {
    fn from(h: HourTokens) -> Self {
        Self {
            input: h.input,
            output: h.output,
            cache_read: h.cache_read,
            cache_create: h.cache_create,
        }
    }
}

impl From<WireHour> for HourTokens {
    fn from(w: WireHour) -> Self {
        Self {
            input: w.input,
            output: w.output,
            cache_read: w.cache_read,
            cache_create: w.cache_create,
        }
    }
}

/// Durable per-day token totals, persisted across processes.
#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Ledger {
    /// Latest day (`YYYY-MM-DD`) whose totals are final in `days`. Every calendar
    /// day at or before this is accounted for — a day with no usage is simply
    /// absent from `days` and contributes nothing. `None` until the first record.
    recorded_through: Option<String>,
    /// `date -> model -> split`.
    days: HashMap<String, HashMap<String, WireModel>>,
    /// Set once the one-shot v1→v2 hourly backfill pass has run (whether it
    /// filled anything or not), so the transcript corpus is swept at most
    /// once per ledger. Absent in files written before the backfill;
    /// `serde(default)` reads it `false`, which is what makes every
    /// pre-upgrade ledger owe exactly one pass.
    #[serde(default)]
    backfill_done: bool,
}

impl Ledger {
    fn path(clauth_dir: &Path) -> PathBuf {
        clauth_dir.join(LEDGER_FILE)
    }

    /// Load the ledger, or an empty one when absent/unreadable/corrupt — the
    /// ledger is a durability + speed layer, never required for a correct base.
    pub(crate) fn load(clauth_dir: &Path) -> Self {
        std::fs::read(Self::path(clauth_dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Persist atomically. Best-effort: a write failure only forfeits the
    /// optimization for one cycle.
    pub(crate) fn save(&self, clauth_dir: &Path) {
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = crate::profile::atomic_write_600(&Self::path(clauth_dir), &bytes);
        }
    }

    /// The transcript sweep's effective cutoff: the later of the base's
    /// `lastComputedDate` and the ledger's `recorded_through`. Days at or before
    /// it are already durable (base or ledger), so the sweep can skip them.
    pub(crate) fn effective_cutoff(&self, last_computed_date: Option<&str>) -> Option<String> {
        last_computed_date
            .into_iter()
            .chain(self.recorded_through.as_deref())
            .max()
            .map(str::to_owned)
    }

    /// Fold the ledger's recorded days into `base` (already holding stats-cache
    /// data), extending `daily`, `daily_models`, `models`, and the totals — the
    /// same shape the top-up produces, so the Tokens views need no ledger
    /// awareness. Each pushed row also carries the stored per-hour buckets when
    /// the ledger day has them. Only days strictly after `last_computed_date`
    /// are folded, so a base that later advances past a ledger day never
    /// double-counts.
    pub(crate) fn apply_to_base(&self, base: &mut TokenStats, last_computed_date: Option<&str>) {
        let floor = last_computed_date.unwrap_or("");
        let mut model_map: HashMap<String, ModelTokens> = base
            .models
            .iter()
            .cloned()
            .map(|m| (m.model.clone(), m))
            .collect();

        for (date, models) in &self.days {
            if date.as_str() <= floor {
                continue; // stats-cache already covers this day
            }
            let mut day_in_out = 0u64;
            for (model, w) in models {
                let split = ModelTokens {
                    model: model.clone(),
                    input: w.input,
                    output: w.output,
                    cache_read: w.cache_read,
                    cache_create: w.cache_create,
                };
                day_in_out = day_in_out.saturating_add(split.in_out());
                base.daily_models.push(DayModelTokens {
                    date: date.clone(),
                    model: model.clone(),
                    in_out: split.in_out(),
                    split: Some(split.clone()),
                    hours: w.hours.as_ref().map(|hs| hs.map(HourTokens::from)),
                });
                let e = model_map
                    .entry(model.clone())
                    .or_insert_with(|| ModelTokens {
                        model: model.clone(),
                        ..Default::default()
                    });
                e.input = e.input.saturating_add(w.input);
                e.output = e.output.saturating_add(w.output);
                e.cache_read = e.cache_read.saturating_add(w.cache_read);
                e.cache_create = e.cache_create.saturating_add(w.cache_create);
            }
            base.daily.push(DayTokens {
                date: date.clone(),
                tokens: day_in_out,
            });
        }

        base.models = model_map.into_values().collect();
        base.models
            .sort_unstable_by_key(|m| std::cmp::Reverse(m.total()));
        base.total_input = base.models.iter().map(|m| m.input).sum();
        base.total_output = base.models.iter().map(|m| m.output).sum();
        base.total_cache_read = base.models.iter().map(|m| m.cache_read).sum();
        base.total_cache_create = base.models.iter().map(|m| m.cache_create).sum();
        base.daily.sort_unstable_by_key(|d| d.date.clone());
        base.daily_models.sort_unstable_by(|a, b| {
            (a.date.as_str(), a.model.as_str()).cmp(&(b.date.as_str(), b.model.as_str()))
        });
    }

    /// Record every finalized day the merged `base` carries a split for, then
    /// advance `recorded_through` to yesterday. A finalized day is any day after
    /// the current watermark and strictly before `today`; the sweep always covers
    /// up to now, so once merged, every day through yesterday is complete. The
    /// watermark is monotonic, so a day is recorded exactly once. Returns whether
    /// anything changed (worth a `save`).
    pub(crate) fn record(&mut self, base: &TokenStats, today: &str) -> bool {
        let floor = self.recorded_through.clone().unwrap_or_default();
        let mut changed = false;

        for d in &base.daily_models {
            let Some(split) = &d.split else { continue };
            if d.date.as_str() <= floor.as_str() || d.date.as_str() >= today {
                continue;
            }
            self.days.entry(d.date.clone()).or_default().insert(
                d.model.clone(),
                WireModel {
                    input: split.input,
                    output: split.output,
                    cache_read: split.cache_read,
                    cache_create: split.cache_create,
                    hours: d.hours.map(|hs| hs.map(WireHour::from)),
                },
            );
            changed = true;
        }

        // Everything through yesterday is now final: advance the watermark even
        // across idle (no-usage) days, which are legitimately absent from `days`.
        if let Some(yesterday) = prev_day(today)
            && self
                .recorded_through
                .as_deref()
                .is_none_or(|r| r < yesterday.as_str())
        {
            self.recorded_through = Some(yesterday);
            changed = true;
        }
        changed
    }

    /// The watermark date the one-shot hourly backfill may sweep up to, when
    /// that pass still has work: the flag unset AND at least one day strictly
    /// before `today` holding `hours: None` rows (every row a pre-v2 ledger
    /// recorded qualifies, so an upgraded ledger owes exactly one pass; after
    /// the pass the flag makes this `None` even when rows did not fill, since
    /// transcripts only ever shrink and a re-sweep could never fill more).
    /// Also `None` when there is no watermark to derive a cutoff from.
    pub(crate) fn backfill_through(&self, today: &str) -> Option<String> {
        if self.backfill_done {
            return None;
        }
        let owed = self.days.iter().any(|(date, models)| {
            date.as_str() < today && models.values().any(|w| w.hours.is_none())
        });
        owed.then(|| self.recorded_through.clone()).flatten()
    }

    /// Fill hour buckets on the ledger's v1 rows from a re-derived transcript
    /// corpus ([`crate::tokens::backfill_corpus`]), exact-or-absent: a row's
    /// `hours` is set ONLY when the corpus totals equal the stored flat totals
    /// on all four fields. Any mismatch means the transcripts no longer fully
    /// cover the day (pruned), and the row is left untouched — stored v1 flat
    /// data is never lowered or rewritten (the equal case is equal by
    /// construction), so a day whose rows do not all fill keeps its v1 shape
    /// forever. Marks [`Ledger::backfill_done`] either way: the sweep runs at
    /// most once per ledger, and a re-sweep could never fill what the first
    /// one left unmatched.
    pub(crate) fn backfill_hours(&mut self, derived: &HashMap<(String, String), ModelDayAcc>) {
        for ((date, model), acc) in derived {
            let Some(day) = self.days.get_mut(date) else {
                continue;
            };
            let Some(w) = day.get_mut(model) else {
                continue;
            };
            if w.hours.is_some() {
                continue; // already on the hourly axis — never rewritten
            }
            if acc.flat.input == w.input
                && acc.flat.output == w.output
                && acc.flat.cache_read == w.cache_read
                && acc.flat.cache_create == w.cache_create
            {
                w.hours = Some(acc.hours.map(WireHour::from));
            }
        }
        self.backfill_done = true;
    }
}

/// The calendar day before `date` (`YYYY-MM-DD`), UTC. `None` on an unparseable
/// input.
fn prev_day(date: &str) -> Option<String> {
    let secs = iso_to_epoch_secs(&format!("{date}T00:00:00+00:00"))?;
    let iso = epoch_secs_to_iso(secs - 86_400);
    iso.get(..10).map(str::to_owned)
}

#[cfg(test)]
#[path = "../tests/inline/token_ledger.rs"]
mod tests;
