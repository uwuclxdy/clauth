//! Durable per-day token ledger for the Tokens tab.
//!
//! # Why
//!
//! The tab's base is `~/.clauth/stats-base.json`, a frozen copy of CC's
//! `stats-cache.json`, authoritative only up to its `lastComputedDate`; the
//! [`crate::tokens`] top-up bridges the gap by reading `~/.claude/projects/`
//! transcripts strictly newer than that date. That bridge is load-bearing
//! precisely because the base stays frozen for good — but CC also prunes
//! transcripts past `cleanupPeriodDays`. Once a day sits BOTH after a
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
//!   `lastComputedDate`, so the ledger never double-counts against the base. The
//!   global `stats-cache.json` catching up past a ledger day changes nothing: the
//!   frozen copy's date is the one compared, and ledger days keep their cache
//!   split, hours and served-model correction.
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
//!
//! Each hour bucket also splits its four totals into a fast-mode subset (the
//! lines whose `usage.speed` was `fast`): a `when.mode = "fast"` price-feed
//! override prices that subset at the fast rate. The subset keys are absent
//! on the wire when 0, so a ledger written before this axis existed loads
//! with every fast bucket 0 — those days price as the standard lines they
//! were, never re-guessed — and is re-saved byte-identically.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pricing::HourTokens;
use crate::tokens::{DayModelTokens, DayTokens, ModelDayAcc, ModelTokens, TokenStats, UsageShape};
use crate::usage::{epoch_secs_to_iso, iso_to_epoch_secs};

const LEDGER_FILE: &str = "token_ledger.json";

/// The speed ledger's own file (`~/.clauth/token_speed.json`).
const SPEED_LEDGER_FILE: &str = "token_speed.json";

/// The speed ledger's schema version.
const SPEED_LEDGER_VERSION: u16 = 2;

/// The usage-shape classifier version this build re-derives recorded days
/// under. Each classifier change bumps it; every ledger stamped below it owes
/// exactly one re-derive pass.
pub(crate) const SHAPE_CLF_VERSION: u16 = 3;

/// The served-model map version this build re-derives recorded days under
/// ([`crate::providers::deepseek_served_model`]). Each map change bumps it;
/// every ledger stamped below it owes exactly one re-derive pass.
pub(crate) const MODEL_MAP_VERSION: u16 = 1;

/// One model's stored split for one day (mirrors [`ModelTokens`] without the
/// redundant `model` name, which is the map key). `hours` is the schema-v2
/// hourly axis.
#[derive(Serialize, Deserialize, Default)]
struct WireModel {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    /// The fast-mode subset of the four fields above, per [`WireHour`]: a file
    /// written before the ledger carried a line's serving mode loads with
    /// every fast field 0 — those tokens price as the standard lines they
    /// were. A row with no fast tokens writes no fast keys, so a re-saved old
    /// day stays byte-identical on the wire.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    fast_input: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    fast_output: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    fast_cache_read: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    fast_cache_create: u64,
    /// How the model's usage rows reported cache, per the shape classifier.
    /// Absent in files written before the classifier: `serde(default)` reads
    /// it [`UsageShape::Healthy`], which never corrects — so a pre-classifier
    /// row keeps rendering as before.
    #[serde(default, skip_serializing_if = "is_default_shape")]
    shape: UsageShape,
    /// Per-hour buckets, index = hour 0..23. A v1 file (no `hours` key) loads
    /// with `None` — serde leaves an absent `Option` field `None`; the explicit
    /// `default` is belt-and-braces per the schema contract. `skip_serializing_if`
    /// keeps a v1 day's wire shape byte-for-byte v1 on save, so the file only
    /// gains `hours` entries as new days are recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hours: Option<[WireHour; 24]>,
}

fn is_default_shape(s: &UsageShape) -> bool {
    *s == UsageShape::Healthy
}

/// One hour's token buckets on the wire — the serde twin of [`HourTokens`]
/// (which deliberately carries no serde derives). The `fast_*` fields default
/// to 0 on the wire: a file written before the ledger carried a line's
/// serving mode loads with every fast bucket 0 — those days price as the
/// standard lines they were. A bucket with no fast tokens writes no fast
/// keys, so a re-saved old day stays byte-identical on the wire.
#[derive(Serialize, Deserialize, Clone, Copy, Default)]
struct WireHour {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    fast_input: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    fast_output: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    fast_cache_read: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    fast_cache_create: u64,
}

/// `skip_serializing_if` for the fast sub-buckets: a bucket with no fast
/// tokens writes no fast keys.
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl From<HourTokens> for WireHour {
    fn from(h: HourTokens) -> Self {
        Self {
            input: h.input,
            output: h.output,
            cache_read: h.cache_read,
            cache_create: h.cache_create,
            fast_input: h.fast_input,
            fast_output: h.fast_output,
            fast_cache_read: h.fast_cache_read,
            fast_cache_create: h.fast_cache_create,
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
            fast_input: w.fast_input,
            fast_output: w.fast_output,
            fast_cache_read: w.fast_cache_read,
            fast_cache_create: w.fast_cache_create,
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
    /// Version of the usage-shape classifier the recorded days were last
    /// re-derived under. Absent (0) in every file written before the field
    /// existed — including v1 files whose `rederive_done` flag read true, a
    /// key serde now ignores — so each older ledger owes exactly one pass
    /// under the current classifier.
    #[serde(default)]
    shape_clf: u16,
    /// Version of the served-model map the recorded days were last re-derived
    /// under. Absent (0) in every file written before the field existed, so
    /// each older ledger owes exactly one pass.
    #[serde(default)]
    model_map: u16,
}

impl Ledger {
    fn path(clauth_dir: &Path) -> PathBuf {
        clauth_dir.join(LEDGER_FILE)
    }

    /// Load the ledger, or an empty one when absent/unreadable/corrupt — the
    /// ledger is a durability + speed layer, never required for a correct base.
    /// An empty one records every day through the current served-model map,
    /// so it starts stamped and owes no re-key.
    pub(crate) fn load(clauth_dir: &Path) -> Self {
        let mut ledger: Self = std::fs::read(Self::path(clauth_dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| Self {
                model_map: MODEL_MAP_VERSION,
                ..Self::default()
            });
        for models in ledger.days.values_mut() {
            models.retain(|model, _| !crate::tokens::is_placeholder_model(model));
        }
        ledger
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
    /// are folded, so a base past a ledger day never double-counts it.
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
                    fast_input: w.fast_input,
                    fast_output: w.fast_output,
                    fast_cache_read: w.fast_cache_read,
                    fast_cache_create: w.fast_cache_create,
                    shape: w.shape,
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
                e.fast_input = e.fast_input.saturating_add(w.fast_input);
                e.fast_output = e.fast_output.saturating_add(w.fast_output);
                e.fast_cache_read = e.fast_cache_read.saturating_add(w.fast_cache_read);
                e.fast_cache_create = e.fast_cache_create.saturating_add(w.fast_cache_create);
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
                    fast_input: split.fast_input,
                    fast_output: split.fast_output,
                    fast_cache_read: split.fast_cache_read,
                    fast_cache_create: split.fast_cache_create,
                    shape: split.shape,
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
    /// forever. The fill also adopts the corpus's fast subset beside the
    /// hours: the corpus re-derived the same lines, so the subset is exact,
    /// and flat and buckets land on one fast axis. Marks
    /// [`Ledger::backfill_done`] either way: the sweep runs at most once per
    /// ledger, and a re-sweep could never fill what the first one left
    /// unmatched.
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
                // The corpus re-derived exactly the stored split, so its fast
                // subset is exact for the same lines — adopt it beside the
                // hours, keeping flat and buckets on one fast axis.
                w.fast_input = acc.flat.fast_input;
                w.fast_output = acc.flat.fast_output;
                w.fast_cache_read = acc.flat.fast_cache_read;
                w.fast_cache_create = acc.flat.fast_cache_create;
                w.hours = Some(acc.hours.map(WireHour::from));
            }
        }
        self.backfill_done = true;
    }

    /// The watermark date the one-shot shape re-derive may sweep up to, when
    /// that pass still has work: `shape_clf` below [`SHAPE_CLF_VERSION`] and
    /// at least one day strictly before `today`. Every row re-checks,
    /// whole-prompt-marked ones included — a wrong classifier stamps wrong
    /// markers, so no stored shape short-circuits the pass. Also `None` when
    /// there is no watermark to derive a cutoff from.
    pub(crate) fn rederive_through(&self, today: &str) -> Option<String> {
        if self.shape_clf >= SHAPE_CLF_VERSION {
            return None;
        }
        let owed = self.days.keys().any(|date| date.as_str() < today);
        owed.then(|| self.recorded_through.clone()).flatten()
    }

    /// Correct the stored days from a re-derived transcript corpus
    /// ([`crate::tokens::backfill_corpus`], whose parse classifies + corrects
    /// per (file, model)). Coverage-gated: a stored row whose re-derivation
    /// equals it on output, cache-read and cache-create was built from the
    /// same rows, so whatever its `input` delta is, it is pure classification
    /// — adopt the re-derived input, shape and hours. No per-direction
    /// arithmetic can gate this: a mixed day (some files legitimately
    /// corrected, some over- or under-corrected by an earlier classifier)
    /// matches no exact relation against the stored split, so the v2
    /// re-derive left exactly those days uncorrected. Any other mismatch
    /// means the corpus no longer covers the day (pruned): the row keeps its
    /// recorded values and gets the [`UsageShape::Healthy`] marker, which
    /// never corrects — unverifiable rather than silently corrected. Days the
    /// corpus cannot reach at all stay entirely untouched (same marker
    /// rule). Stamps [`SHAPE_CLF_VERSION`] either way.
    pub(crate) fn rederive_shapes(&mut self, derived: &HashMap<(String, String), ModelDayAcc>) {
        for ((date, model), acc) in derived {
            let Some(day) = self.days.get_mut(date) else {
                continue;
            };
            let Some(w) = day.get_mut(model) else {
                continue;
            };
            if acc.flat.output == w.output
                && acc.flat.cache_read == w.cache_read
                && acc.flat.cache_create == w.cache_create
            {
                w.input = acc.flat.input;
                w.fast_input = acc.flat.fast_input;
                w.fast_output = acc.flat.fast_output;
                w.fast_cache_read = acc.flat.fast_cache_read;
                w.fast_cache_create = acc.flat.fast_cache_create;
                w.hours = Some(acc.hours.map(WireHour::from));
                w.shape = acc.flat.shape;
            }
            // Any other mismatch: unverifiable — keep values, keep the
            // never-correcting `Healthy` marker.
        }
        self.shape_clf = SHAPE_CLF_VERSION;
    }

    /// The watermark date the one-shot served-model re-derive may sweep up
    /// to, when that pass still has work: `model_map` below
    /// [`MODEL_MAP_VERSION`] and at least one day strictly before `today`.
    /// Also `None` when there is no watermark to derive a cutoff from.
    pub(crate) fn remap_through(&self, today: &str) -> Option<String> {
        if self.model_map >= MODEL_MAP_VERSION {
            return None;
        }
        let owed = self.days.keys().any(|date| date.as_str() < today);
        owed.then(|| self.recorded_through.clone()).flatten()
    }

    /// Re-key the stored days onto the models a re-derived transcript corpus
    /// ([`crate::tokens::backfill_corpus`]) counts each line under. Only the
    /// keys the map can move take part
    /// ([`crate::providers::deepseek_served_model_may_move`]); every other
    /// row stays as recorded. A map change moves tokens between those keys
    /// inside a day, so no stored row pairs with one re-derived row: the gate
    /// is the day's movable rows taken together. When their re-derived sums
    /// equal the stored ones on all four fields, the corpus still holds every
    /// movable row the day was recorded from, and those rows become the
    /// re-derived split. A day over or short on any field (pruned, or a
    /// regrouped model classified differently) keeps its recorded rows, and
    /// so does a day the map leaves unchanged. Stamps [`MODEL_MAP_VERSION`]
    /// either way.
    pub(crate) fn remap_models(&mut self, derived: &HashMap<(String, String), ModelDayAcc>) {
        let movable = crate::providers::deepseek_served_model_may_move;
        let mut by_day: HashMap<&str, Vec<(&str, &ModelDayAcc)>> = HashMap::new();
        for ((date, model), acc) in derived {
            if movable(model) {
                by_day.entry(date).or_default().push((model, acc));
            }
        }
        for (date, rows) in by_day {
            let Some(day) = self.days.get_mut(date) else {
                continue;
            };
            let stored: Vec<(&str, [u64; 4])> = day
                .iter()
                .filter(|(model, _)| movable(model))
                .map(|(model, w)| {
                    (
                        model.as_str(),
                        [w.input, w.output, w.cache_read, w.cache_create],
                    )
                })
                .collect();
            let rederived: Vec<(&str, [u64; 4])> = rows
                .iter()
                .map(|(model, a)| {
                    (
                        *model,
                        [
                            a.flat.input,
                            a.flat.output,
                            a.flat.cache_read,
                            a.flat.cache_create,
                        ],
                    )
                })
                .collect();
            if sums(&stored) != sums(&rederived) {
                continue;
            }
            let unchanged =
                stored.len() == rederived.len() && rederived.iter().all(|row| stored.contains(row));
            if unchanged {
                continue;
            }
            day.retain(|model, _| !movable(model));
            day.extend(rows.into_iter().map(|(model, a)| {
                (
                    model.to_owned(),
                    WireModel {
                        input: a.flat.input,
                        output: a.flat.output,
                        cache_read: a.flat.cache_read,
                        cache_create: a.flat.cache_create,
                        fast_input: a.flat.fast_input,
                        fast_output: a.flat.fast_output,
                        fast_cache_read: a.flat.fast_cache_read,
                        fast_cache_create: a.flat.fast_cache_create,
                        shape: a.flat.shape,
                        hours: Some(a.hours.map(WireHour::from)),
                    },
                )
            }));
        }
        self.model_map = MODEL_MAP_VERSION;
    }

    /// Test-only: the shape-classifier version the days were re-derived under.
    #[cfg(test)]
    pub(crate) fn shape_clf(&self) -> u16 {
        self.shape_clf
    }

    /// Test-only: one stored day/model row's fields.
    #[cfg(test)]
    pub(crate) fn wire_model_fields(
        &self,
        date: &str,
        model: &str,
    ) -> Option<(u64, u64, u64, u64, UsageShape, bool)> {
        self.days.get(date)?.get(model).map(|w| {
            (
                w.input,
                w.output,
                w.cache_read,
                w.cache_create,
                w.shape,
                w.hours.is_some(),
            )
        })
    }
}

/// Field-wise sums of `(model, [input, output, cache_read, cache_create])`
/// rows.
fn sums(rows: &[(&str, [u64; 4])]) -> [u64; 4] {
    rows.iter().fold([0; 4], |total, (_, row)| {
        std::array::from_fn(|i| total[i].saturating_add(row[i]))
    })
}

/// The calendar day before `date` (`YYYY-MM-DD`), UTC. `None` on an unparseable
/// input.
fn prev_day(date: &str) -> Option<String> {
    let secs = iso_to_epoch_secs(&format!("{date}T00:00:00+00:00"))?;
    let iso = epoch_secs_to_iso(secs - 86_400);
    iso.get(..10).map(str::to_owned)
}

/// Per-day tps speed sums in `~/.clauth/token_speed.json`, apart from the
/// token ledger so an older clauth saving that file never drops them.
#[derive(Debug, Default)]
pub(crate) struct SpeedLedger {
    pub(crate) days: crate::tps::DaySums,
    /// Per-day rate peaks (the busiest 10 s window per grouping), the `ever`
    /// half's rate basis. Rates never aggregate by max.
    pub(crate) rate_peaks: BTreeMap<String, crate::tps::DayRatePeaks>,
    /// Newer version: never save over it and never retry-reload.
    newer: bool,
    /// Unreadable (a read error): never save over it, but the producer may
    /// retry-reload when its file changes or on a timer.
    unreadable: bool,
    /// The on-disk file does not parse; rename it aside before the first save.
    corrupt: bool,
    /// The rename-aside failure was already logged.
    rename_logged: bool,
    /// The file on disk is not this version's (absent, older, or
    /// unparseable): [`SpeedLedger::claim`] saves it as this version.
    outdated: bool,
    /// This life's latest save of the ledger landed.
    landed: bool,
    /// The content a save of this ledger alone built and could not land,
    /// kept until a save lands so a retry only writes it again: the content
    /// changes only through a fold that landed.
    unsaved_own: Option<Built>,
    /// The same for a fold's save, keyed by the days and rate peaks it merged
    /// in: another fold's content is built anew.
    unsaved_fold: Option<Built>,
}

/// A save's content, with the days and rate peaks it merged into the ledger.
#[derive(Debug)]
struct Built {
    days: crate::tps::DaySums,
    rate_peaks: BTreeMap<String, crate::tps::DayRatePeaks>,
    bytes: Vec<u8>,
}

/// What [`SpeedLedger::fold`] did with a fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldOutcome {
    /// Merged and saved: the ledger on disk holds it.
    Saved,
    /// The ledger already held every cell and peak at least as large: nothing
    /// to merge or save.
    Unchanged,
    /// The save did not land (or the file must never be saved over): the
    /// ledger is unchanged.
    Unsaved,
}

/// The speed ledger's wire shape: `{version: 2, days: {date: {account: {model:
/// {output, gen_ms, speed}}}}, rate_peaks: {date: {total, accounts, models}}}`.
/// A v1 file (no `speed`/`rate_peaks` keys) loads with every peak absent and
/// is saved back as v2; a newer version is never saved over.
#[derive(Deserialize, Default)]
struct SpeedLedgerWire {
    #[serde(default)]
    version: u16,
    #[serde(default)]
    days: crate::tps::DaySums,
    #[serde(default)]
    rate_peaks: BTreeMap<String, crate::tps::DayRatePeaks>,
}

/// [`SpeedLedgerWire`] borrowed, for the save; built only by
/// [`SpeedLedger::content`].
#[derive(Serialize)]
struct SpeedLedgerWireRef<'a> {
    version: u16,
    days: BTreeMap<&'a str, &'a BTreeMap<String, BTreeMap<String, crate::tps::DaySum>>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    rate_peaks: BTreeMap<&'a str, &'a crate::tps::DayRatePeaks>,
}

impl SpeedLedger {
    pub(crate) fn load(clauth_dir: &Path) -> Self {
        let path = clauth_dir.join(SPEED_LEDGER_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    outdated: true,
                    ..Self::default()
                };
            }
            Err(_) => {
                // Any other read error is not "absent": never save over a file
                // we could not read. The producer logs this once.
                return Self {
                    unreadable: true,
                    ..Self::default()
                };
            }
        };
        let wire = match serde_json::from_slice::<SpeedLedgerWire>(&bytes) {
            Ok(w) => w,
            Err(_) => {
                // Never save over an unparsable file: its rows are not
                // re-derivable once the transcripts age out.
                return Self {
                    corrupt: true,
                    outdated: true,
                    ..Self::default()
                };
            }
        };
        if wire.version > SPEED_LEDGER_VERSION {
            return Self {
                newer: true,
                ..Self::default()
            };
        }
        Self {
            days: wire.days,
            rate_peaks: wire.rate_peaks,
            outdated: wire.version < SPEED_LEDGER_VERSION,
            ..Self::default()
        }
    }

    /// Save a file that is not this version's as this version, days kept
    /// (an unparseable one moves aside first), so an older clauth reads it as
    /// newer and never folds into it a day this build later refolds. The
    /// producer calls it every tick; it writes only until a
    /// save lands, and never over an unusable file.
    pub(crate) fn claim(&mut self, clauth_dir: &Path) {
        if self.outdated && !self.unusable() {
            self.save(clauth_dir);
        }
    }

    /// True when a ledger save lands: at once when this life's latest save
    /// landed, else after one save of the ledger as this life holds it (the
    /// days on disk, at this version). The producer asks before an owed-day
    /// rebuild, so a start whose saves fail (a read-only `~/.clauth`) never
    /// pays the rebuild for a fold that cannot land.
    pub(crate) fn saves_land(&mut self, clauth_dir: &Path) -> bool {
        self.landed || self.save(clauth_dir)
    }

    /// True when the on-disk file must never be saved over (newer version or
    /// unreadable).
    pub(crate) fn unusable(&self) -> bool {
        self.newer || self.unreadable
    }

    /// True when the file is a newer version (never retry-reload).
    pub(crate) fn newer(&self) -> bool {
        self.newer
    }

    /// Fold the producer's finalized days (strictly before `today`) into the
    /// ledger and save it: a day the ledger holds refolds per model by
    /// [`crate::tps::refold_day`] (the larger model total wins, its account
    /// split whole), and every speed and rate peak takes the max, so no clock
    /// step ever lowers a stored day's model total or drops a peak. Nothing is
    /// written when nothing changed. The merged days replace this ledger's
    /// only once the save landed, so a failed save leaves the ledger exactly
    /// as on disk.
    pub(crate) fn fold(
        &mut self,
        clauth_dir: &Path,
        days: &crate::tps::DaySums,
        rate_peaks: &BTreeMap<String, crate::tps::DayRatePeaks>,
        today: &str,
    ) -> FoldOutcome {
        let mut changed = false;
        let mut merged_days = crate::tps::DaySums::new();
        for (date, accounts) in days.iter().filter(|(d, _)| d.as_str() < today) {
            let mut day = self.days.get(date).cloned().unwrap_or_default();
            changed |= crate::tps::refold_day(&mut day, accounts);
            merged_days.insert(date.clone(), day);
        }
        let mut merged_peaks = BTreeMap::new();
        for (date, peaks) in rate_peaks.iter().filter(|(d, _)| d.as_str() < today) {
            let held = self.rate_peaks.get(date).cloned().unwrap_or_default();
            let merged = crate::tps::merge_rate_peaks(held.clone(), peaks.clone());
            changed |= merged != held;
            merged_peaks.insert(date.clone(), merged);
        }
        if !changed {
            return FoldOutcome::Unchanged;
        }
        if self.save_merged(clauth_dir, &merged_days, &merged_peaks) {
            self.days.extend(merged_days);
            self.rate_peaks.extend(merged_peaks);
            FoldOutcome::Saved
        } else {
            FoldOutcome::Unsaved
        }
    }

    /// Persist atomically (0600). Returns whether the save succeeded. An
    /// unusable file is never touched (returns false). An unparsable file is
    /// renamed aside once (0600) before the first save; a failed rename skips
    /// the write and returns false, leaving the corrupt file in place.
    pub(crate) fn save(&mut self, clauth_dir: &Path) -> bool {
        self.save_merged(clauth_dir, &crate::tps::DaySums::new(), &BTreeMap::new())
    }

    /// [`SpeedLedger::save`] of this ledger with `days` and `rate_peaks`
    /// replacing its own entries for their dates, leaving `self`'s days as
    /// they are. The content is built only once the temp file opens, so a
    /// save refused there costs the same at any ledger size: the producer
    /// retries a failing save every tick (claim, the rebuild's
    /// [`SpeedLedger::saves_land`], a held day's fold) and notices a
    /// writable `~/.clauth` on the first tick it is one. A save failing after
    /// the temp opens (a full disk, a refused rename) keeps the content it
    /// built, so the retry of the same content only writes it again.
    fn save_merged(
        &mut self,
        clauth_dir: &Path,
        days: &crate::tps::DaySums,
        rate_peaks: &BTreeMap<String, crate::tps::DayRatePeaks>,
    ) -> bool {
        self.landed = false;
        if self.unusable() {
            return false;
        }
        let path = clauth_dir.join(SPEED_LEDGER_FILE);
        if self.corrupt {
            let aside = clauth_dir.join(format!(
                "token_speed.json.corrupt-{}",
                crate::usage::now_ms()
            ));
            if std::fs::rename(&path, &aside).is_err() {
                if !self.rename_logged {
                    crate::logline::logline!(
                        "clauth: could not move the unreadable token_speed.json aside; not saving over it"
                    );
                    self.rename_logged = true;
                }
                return false;
            }
            self.rename_logged = false;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o600));
            }
            crate::logline::logline!(
                "clauth: token_speed.json was unreadable; moved aside to {}",
                aside.display()
            );
            self.corrupt = false;
        }
        let own = days.is_empty() && rate_peaks.is_empty();
        let mut kept = if own {
            self.unsaved_own.take()
        } else {
            self.unsaved_fold.take()
        }
        .filter(|b| b.days == *days && b.rate_peaks == *rate_peaks);
        let saved = crate::profile::atomic_write_600_with(&path, |f| {
            let built = match kept.take() {
                Some(built) => built,
                None => Built {
                    bytes: self.content(days, rate_peaks)?,
                    days: days.clone(),
                    rate_peaks: rate_peaks.clone(),
                },
            };
            let written = std::io::Write::write_all(f, &built.bytes);
            kept = Some(built);
            written
        })
        .is_ok();
        if saved {
            self.outdated = false;
            self.unsaved_own = None;
            self.unsaved_fold = None;
        } else if own {
            self.unsaved_own = kept;
        } else {
            self.unsaved_fold = kept;
        }
        self.landed = saved;
        saved
    }

    /// The file's bytes for this ledger with `days` and `rate_peaks` replacing
    /// its own entries for their dates: the one place a save's content is
    /// built.
    fn content(
        &self,
        days: &crate::tps::DaySums,
        rate_peaks: &BTreeMap<String, crate::tps::DayRatePeaks>,
    ) -> serde_json::Result<Vec<u8>> {
        #[cfg(test)]
        serializations::count();
        let mut wire = SpeedLedgerWireRef {
            version: SPEED_LEDGER_VERSION,
            days: self.days.iter().map(|(d, a)| (d.as_str(), a)).collect(),
            rate_peaks: self
                .rate_peaks
                .iter()
                .map(|(d, p)| (d.as_str(), p))
                .collect(),
        };
        wire.days.extend(days.iter().map(|(d, a)| (d.as_str(), a)));
        wire.rate_peaks
            .extend(rate_peaks.iter().map(|(d, p)| (d.as_str(), p)));
        serde_json::to_vec(&wire)
    }

    /// The speed rows summed across accounts (incl. `*`) and models per day:
    /// `date -> (output, gen_ms)`. Avg speed = output ÷ seconds.
    pub(crate) fn speed_days(&self) -> std::collections::BTreeMap<String, crate::tps::DaySum> {
        let mut out = std::collections::BTreeMap::new();
        for (date, accounts) in &self.days {
            let mut day = crate::tps::DaySum::default();
            for models in accounts.values() {
                for w in models.values() {
                    day.output = day.output.saturating_add(w.output);
                    day.gen_ms = day.gen_ms.saturating_add(w.gen_ms);
                }
            }
            out.insert(date.clone(), day);
        }
        out
    }
}

/// Test seam: how many times this thread built a speed ledger's content for
/// a save, so a test can tell a save refused before the content from one
/// that built it.
#[cfg(test)]
pub(crate) mod serializations {
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn count() {
        COUNT.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn taken() -> u64 {
        COUNT.with(Cell::get)
    }
}

#[cfg(test)]
#[path = "../tests/inline/token_ledger.rs"]
mod tests;
