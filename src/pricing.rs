//! Model price table — fetches per-token USD rates so the Tokens tab can show
//! API-equivalent cost (what the recorded usage *would* cost at pay-as-you-go
//! API rates; clauth users are on subscription plans, so this reads as "value
//! extracted", not a bill).
//!
//! # Source
//!
//! The ai-pricelog index (`data/index.json` on the `mommy` branch of
//! uwuclxdy/ai-pricelog, version 3): a `sources` object mapping provider name
//! → model id → a flat rate row (`input_mtok`, `output_mtok`,
//! `cache_read_mtok`, `cache_write_mtok`, USD per million tokens). A row may
//! carry `effective_at` (prices apply from that date on; earlier dates price
//! nothing) and `window_rates` entries (an `[HHMM, HHMM]` window, an optional
//! UTC-weekday `days` set, and override rate keys whose absent fields inherit
//! the row's base price; later entries override earlier matching ones). A row
//! carrying `removed_at` is delisted and prices nothing. Only first-party
//! providers are kept; resellers (OpenRouter, Novita, Together, Avian, …) are
//! dropped, so a bare id never prices through a reseller's markup. Resold
//! model rows inside a kept provider (a claude id under a non-anthropic
//! provider) are dropped the same way.
//!
//! # Design (mirrors `status.rs`)
//!
//! TUI-free: owns the data model, the HTTP fetch, the distill step, and the
//! on-disk cache, but never touches ratatui. A background thread cold-loads the
//! disk cache (so cost renders instantly and offline once primed), then fetches
//! the live feed and refreshes on a slow cadence — prices change rarely. The UI
//! thread reads [`PricingEvent`]s and holds the latest [`PriceTable`]; no shared
//! lock crosses the thread boundary, only the channel does.
//!
//! Every successful fetch appends a snapshot to the table's history (skipped
//! when the distilled models are byte-identical to the last snapshot's, capped
//! at [`HISTORY_CAP`] snapshots), so a past day re-prices at the rates live on
//! that day. Snapshot selection for a query date: the newest snapshot with
//! `captured <= date`; a date older than every snapshot uses the oldest one.
//!
//! # Cost basis
//!
//! Cost is computed **per model** and summed — never via a blended rate, since
//! family rates differ up to 10× (Opus $5/$25 vs Haiku $1/$5 per 1M). It always
//! counts cache tokens (they cost real money on the API), independent of the
//! Tokens tab's `count_cache` display toggle. Models with no matching rate
//! (unknown / unpriced providers) contribute nothing and are surfaced as such.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use chrono::{Datelike, NaiveDate, Weekday};
use serde::{Deserialize, Serialize};

use crate::logline::logline;
use crate::poll::{first_delay, run_polling_loop};
use crate::profile::{atomic_write_600, clauth_dir};
use crate::tokens::{ModelTokens, today_date};
use crate::usage::now_ms;

/// Live price feed (the ai-pricelog index, fetched from GitHub raw `mommy`).
const FEED_URL: &str =
    "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/mommy/data/index.json";

/// Background refresh cadence. Prices move rarely, so this is deliberately slow;
/// a manual refresh signal short-circuits the wait.
const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// HTTP connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// HTTP response-receive timeout.
const RECV_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard cap on the response body. The real feed is ~298 KiB; 8 MiB is generous
/// headroom while still bounding a hostile / runaway response.
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// Snapshot history cap: the newest 180 fetches survive, older ones drop.
const HISTORY_CAP: usize = 180;

/// The ai-pricelog index version this table parses. An unknown version warns
/// through [`logline!`] and parses best-effort.
const INDEX_VERSION: u64 = 3;

/// First-party providers distilled into the table. Every other provider id in
/// the index resells another vendor's models (openrouter, novita, together,
/// avian, baseten, cloudflare, deepinfra, …); keeping them would let a bare id
/// price through a reseller's markup. zhipuai and voyageai are closed upstream
/// (their pages no longer publish rates) and stay out.
const FIRST_PARTY_PROVIDERS: &[&str] = &[
    "anthropic",
    "deepseek",
    "zai",
    "minimax",
    "moonshotai",
    "x-ai",
    "openai",
    "google",
    "mistral",
    "groq",
    "cohere",
    "cerebras",
    "perplexity",
    "dashscope",
];

// ── Data model ──────────────────────────────────────────────────────────────

/// Per-token USD rates for one model — a RESOLVED flat rate (the outcome of
/// picking the active [`PriceEntry`]). `cache_write` is the 5-minute-TTL
/// creation rate (the common case; the 1-hour rate is not modeled — the hourly
/// axis has no TTL data). Missing upstream fields (e.g. a provider with no
/// cache-write rate) default to `0.0`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct ModelRate {
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cache_read: f64,
    pub(crate) cache_write: f64,
}

/// When a [`PriceEntry`] applies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Constraint {
    /// Active inside this daily interval; see [`window_contains`] for the
    /// hour-granularity semantics.
    TimeWindow { start: String, end: String }, // "HH:MM"
    /// Active only on the listed UTC weekdays (lowercase English names, as the
    /// feed's `days` sets spell them), and — when a window is present — inside
    /// it. Days without a window are active all day on those weekdays.
    Days {
        days: Vec<String>,
        start: Option<String>,
        end: Option<String>,
    },
}

/// One price row: the four per-token rates plus an optional constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PriceEntry {
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cache_read: f64,
    pub(crate) cache_write: f64,
    pub(crate) constraint: Option<Constraint>,
}

impl PriceEntry {
    /// Whether this entry is the pick at `(date, hour)`: unconstrained entries
    /// are always active; a constraint must hold. A constraint whose strings do
    /// not parse is never active, so entry selection falls through to the
    /// unconstrained base entry instead of failing the whole table.
    fn active(&self, date: &str, hour: u8) -> bool {
        match &self.constraint {
            None => true,
            Some(Constraint::TimeWindow { start, end }) => window_contains(start, end, hour),
            Some(Constraint::Days { days, start, end }) => {
                let weekday = date_weekday(date).is_some_and(|w| days.iter().any(|d| d == w));
                weekday
                    && match (start, end) {
                        (Some(s), Some(e)) => window_contains(s, e, hour),
                        (None, None) => true,
                        (_, _) => false,
                    }
            }
        }
    }
}

/// One distilled model: its id (the index row's `model_id` — the id IS the
/// match, since the feed carries one row per model id) plus its price entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PricedModel {
    pub(crate) id: String,
    pub(crate) prices: Vec<PriceEntry>,
    /// The row's `effective_at`: prices apply from this date on, inclusive;
    /// [`entry_rate`] returns `None` before it.
    #[serde(default)]
    pub(crate) effective_at: Option<String>,
}

/// One fetch's distilled table: the capture date plus the models live then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RateSnapshot {
    /// Capture date as "YYYY-MM-DD".
    pub(crate) captured: String,
    pub(crate) models: Vec<PricedModel>,
}

/// Per-hour token buckets behind [`PriceTable::cost_day`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HourTokens {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_create: u64,
}

/// Resolved price table: the newest snapshot's models, the full snapshot
/// history (oldest first), and the wall-clock time the feed was fetched (for a
/// freshness badge).
#[derive(Debug)]
pub(crate) struct PriceTable {
    /// Latest snapshot's models — the working set for "today" queries.
    models: Vec<PricedModel>,
    /// Oldest-first snapshot history; [`PriceTable::models_for`] picks the
    /// one applicable to a query date.
    history: Vec<RateSnapshot>,
    pub(crate) fetched_at_ms: u64,
    /// Memoized match walks (see [`Memo`]). A table is immutable once built, so
    /// a remembered index cannot go stale.
    memo: Mutex<Memo>,
}

/// Match walks already done, `date → model id → index into the slice
/// [`PriceTable::models_for`] serves that date`.
///
/// A walk strips and retries the id and scans every model's id once per
/// candidate form, and the cost lens asks for the same `(id, date)` at all 24
/// hours of a day on every frame — the weekly lens measured `days × 24` walks
/// per model per frame on 2026-08-20. The pick is hour-independent (the walk
/// chooses the MODEL, an entry's constraint chooses that model's RATE), so one
/// walk answers every hour, and the memo carries it across frames.
#[derive(Debug, Default)]
struct Memo {
    by_date: HashMap<String, HashMap<String, Option<usize>>>,
    /// Cold walks performed. The memo's only observable, so the tests that pin
    /// "once per (model, date)" have something to count.
    #[cfg(test)]
    walks: usize,
}

impl PriceTable {
    /// Fold a successful fetch into a table: stamp `fetched_at_ms`, append a
    /// snapshot dated `captured` ONLY when the distilled models differ from the
    /// last snapshot's (serialize-and-compare — a byte-identical refetch does
    /// not grow the history), and cap the history at [`HISTORY_CAP`], dropping
    /// the oldest snapshots.
    pub(crate) fn capture(
        models: Vec<PricedModel>,
        captured: String,
        fetched_at_ms: u64,
        mut history: Vec<RateSnapshot>,
    ) -> Self {
        // Serialization of these types cannot fail; on the off chance it does,
        // treating the table as changed only appends one extra snapshot.
        let changed = match serde_json::to_string(&models).ok().zip(
            history
                .last()
                .and_then(|s| serde_json::to_string(&s.models).ok()),
        ) {
            Some((fresh, last)) => fresh != last,
            None => true,
        };
        if changed {
            history.push(RateSnapshot {
                captured,
                models: models.clone(),
            });
            if history.len() > HISTORY_CAP {
                history.drain(..history.len() - HISTORY_CAP);
            }
        }
        Self {
            models,
            history,
            fetched_at_ms,
            memo: Mutex::default(),
        }
    }

    /// Rate for a model id at `(date, hour)`:
    ///
    /// 1. The id is bracket-stripped (a trailing `[<digits>k|m]` context
    ///    suffix, case-insensitive).
    /// 2. The first [`PricedModel`] (in distilled order) whose id equals the
    ///    form, case-insensitively, wins.
    /// 3. Its entries are tried in REVERSE order; the first whose constraint
    ///    is active is the pick — later entries override earlier matching
    ///    ones. A row whose `effective_at` is after `date` prices nothing.
    ///
    /// When no model matches the primary form, derived forms retry in order —
    /// colon strip, then up to two leading `id/` segment strips, then repeated
    /// trailing `-<8 digits>` date-stamp strips — first match wins. Each form
    /// re-enters the full walk, so a retry prices only what a row names; these
    /// restore the spellings the old LiteLLM walk's colon/suffix strips priced
    /// (`minimax/minimax-m2.5:free`, `anthropic/claude-opus-5`). A stripped
    /// form whose spelling no row carries stays unpriced: `glm-4.7-flash-20250801`
    /// strips to `glm-4.7-flash`, which the feed carries no rate for today.
    ///
    /// Steps 1-2 and the retry ladder are [`ladder_index`], memoized per
    /// `(id, date)`; step 3 is [`entry_rate`], the only half the hour reaches.
    ///
    /// Rates come from the snapshot live on `date` (see
    /// [`PriceTable::models_for`]); `None` when no model matches, and `None`
    /// for a matched row whose `effective_at` is after `date`.
    pub(crate) fn rate_at(&self, model: &str, date: &str, hour: u8) -> Option<ModelRate> {
        entry_rate(self.matched(model, date)?, date, hour)
    }

    /// The [`PricedModel`] that prices `model` on `date`, answered from [`Memo`]
    /// when the pair has been asked before. The memoized index is only ever
    /// resolved against the slice this same call took from
    /// [`PriceTable::models_for`], so the index and its date cannot be paired up
    /// wrongly by a caller. A poisoned memo walks rather than failing a price —
    /// it holds derived state and nothing else.
    fn matched(&self, model: &str, date: &str) -> Option<&PricedModel> {
        let models = self.models_for(date)?;
        let Ok(mut memo) = self.memo.lock() else {
            return models.get(ladder_index(models, model)?);
        };
        if let Some(hit) = memo.by_date.get(date).and_then(|by_id| by_id.get(model)) {
            return models.get((*hit)?);
        }
        let found = ladder_index(models, model);
        #[cfg(test)]
        {
            memo.walks += 1;
        }
        memo.by_date
            .entry(date.to_owned())
            .or_default()
            .insert(model.to_owned(), found);
        models.get(found?)
    }

    /// Cold match walks performed so far — what pins the memo, since a warm
    /// answer is otherwise indistinguishable from a repeated walk.
    #[cfg(test)]
    fn walks(&self) -> usize {
        self.memo.lock().expect("memo lock").walks
    }

    /// The models applicable to `date`: the newest snapshot with `captured <=
    /// date` (served straight from `models`, the newest snapshot's working
    /// set); a date older than every snapshot uses the oldest one.
    fn models_for(&self, date: &str) -> Option<&[PricedModel]> {
        if self
            .history
            .last()
            .is_some_and(|s| s.captured.as_str() <= date)
        {
            return Some(&self.models);
        }
        self.history
            .iter()
            .rev()
            .find(|s| s.captured.as_str() <= date)
            .map(|s| s.models.as_slice())
            .or_else(|| self.history.first().map(|s| s.models.as_slice()))
    }

    /// API-equivalent cost in USD for one model's recorded tokens at
    /// `(date, hour)`. `None` when no rate matches (unknown / unpriced model).
    /// Counts all four token buckets.
    pub(crate) fn cost_at(&self, m: &ModelTokens, date: &str, hour: u8) -> Option<f64> {
        let r = self.rate_at(&m.model, date, hour)?;
        Some(
            m.input as f64 * r.input
                + m.output as f64 * r.output
                + m.cache_read as f64 * r.cache_read
                + m.cache_create as f64 * r.cache_write,
        )
    }

    /// Cost of one model across a full day of hourly token buckets, pricing
    /// each hour at its own `(date, hour)` rate (peak/off-peak). `None` when
    /// the model has no matching rate or its row's `effective_at` is after
    /// `date` — a model's match is time-independent, so a match at hour 0
    /// guarantees one at every hour.
    pub(crate) fn cost_day(
        &self,
        model: &str,
        date: &str,
        hours: &[HourTokens; 24],
    ) -> Option<f64> {
        let priced = self.matched(model, date)?;
        let mut total = 0.0;
        for (hour, h) in hours.iter().enumerate() {
            let r = entry_rate(priced, date, hour as u8)?;
            total += h.input as f64 * r.input
                + h.output as f64 * r.output
                + h.cache_read as f64 * r.cache_read
                + h.cache_create as f64 * r.cache_write;
        }
        Some(total)
    }
}

// ── Resolution helpers ──────────────────────────────────────────────────────

/// The candidate-form ladder: which [`PricedModel`] of `models` prices `id`.
/// Hour-independent, which is what lets [`Memo`] answer for all 24 hours of a
/// day; [`PriceTable::rate_at`] documents the ladder itself.
fn ladder_index(models: &[PricedModel], id: &str) -> Option<usize> {
    // Primary form: the id as shipped. The bracket strip applies here and here
    // only — a retried form is not re-stripped.
    let mut candidate = strip_bracket_suffix(id);
    if let Some(i) = form_index(models, candidate) {
        return Some(i);
    }
    // (a) Colon strip: `minimax/minimax-m2.5:free` → `minimax/minimax-m2.5`.
    // The rest of the ladder continues from the stripped form, like the old
    // walk's "retry exact, continue with the stripped id".
    if let Some(idx) = candidate.find(':') {
        candidate = &candidate[..idx];
        if let Some(i) = form_index(models, candidate) {
            return Some(i);
        }
    }
    // (b) Leading provider-segment strip: one `id/` segment at a time, up to
    // two, retrying each intermediate (`openrouter/anthropic/claude-opus-5` →
    // `anthropic/claude-opus-5` → `claude-opus-5`).
    for _ in 0..2 {
        let Some((_, rest)) = candidate.split_once('/') else {
            break;
        };
        candidate = rest;
        if let Some(i) = form_index(models, candidate) {
            return Some(i);
        }
    }
    // (c) Trailing date-stamp strip: while the id ends in `-<8 digits>`, drop
    // that group and retry (`glm-4.7-flash-20250801` → `glm-4.7-flash`;
    // repeated for stacked stamps).
    while let Some(head) = strip_date_stamp(candidate) {
        candidate = head;
        if let Some(i) = form_index(models, candidate) {
            return Some(i);
        }
    }
    None
}

/// One candidate form through the match walk: the first [`PricedModel`] in
/// distilled order whose id equals the form, case-insensitively. An empty
/// `prices` list can price nothing, so a model carrying one is not a match and
/// the ladder moves on to the next form.
fn form_index(models: &[PricedModel], id: &str) -> Option<usize> {
    let idx = models.iter().position(|m| m.id.eq_ignore_ascii_case(id))?;
    (!models.get(idx)?.prices.is_empty()).then_some(idx)
}

/// What one matched model charges at `(date, hour)`: entries in REVERSE order,
/// the first whose constraint is active — later entries override earlier
/// matching ones, so a window entry beats the flat base entry it overlaps.
/// `None` when the row's `effective_at` is after `date` (the row prices
/// nothing yet) or no entry is active. The only half of resolution the hour
/// reaches.
fn entry_rate(priced: &PricedModel, date: &str, hour: u8) -> Option<ModelRate> {
    if priced
        .effective_at
        .as_deref()
        .is_some_and(|effective| date < effective)
    {
        return None;
    }
    let entry = priced.prices.iter().rev().find(|e| e.active(date, hour))?;
    Some(ModelRate {
        input: entry.input,
        output: entry.output,
        cache_read: entry.cache_read,
        cache_write: entry.cache_write,
    })
}

/// Strip ONE trailing `-<exactly 8 digits>` date stamp (a `YYYYMMDD`):
/// `glm-4.7-flash-20250801` → `glm-4.7-flash`. The caller repeats it for
/// stacked stamps. A shorter numeric group (`...-250801`, `...-2508`) or a
/// non-numeric suffix is a model variant name, not a date stamp, and is left
/// alone — a loose `-<segment>` strip would walk past variant names into the
/// family base id and price a different model than the id names.
fn strip_date_stamp(id: &str) -> Option<&str> {
    let (head, tail) = id.rsplit_once('-')?;
    (tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit())).then_some(head)
}

/// Strip a trailing `[<digits>k|m]` context suffix (case-insensitive):
/// `deepseek-v4-pro[1m]` → `deepseek-v4-pro`. Anything else — no closing
/// bracket, no digits, an unknown unit letter — is left alone, so an id with a
/// bracketed segment that is NOT a context suffix still matches its row on
/// the full string.
fn strip_bracket_suffix(id: &str) -> &str {
    let Some(body) = id.strip_suffix(']') else {
        return id;
    };
    let Some((head, unit)) = body.rsplit_once('[') else {
        return id;
    };
    let Some(digits) = unit.strip_suffix(['k', 'K', 'm', 'M']) else {
        return id;
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return id;
    }
    head
}

/// Half-open daily window at HOUR granularity: active when
/// `(start_h, start_m) <= (hour, 0) < (end_h, end_m)` — sampled at the hour's
/// start, like the generator's window semantics. Exact for whole-hour windows
/// (`"24:00"` parses as the exclusive end of the last hour). Unparseable
/// times make the window never active.
fn window_contains(start: &str, end: &str, hour: u8) -> bool {
    let Some((sh, sm)) = parse_hhmm(start) else {
        return false;
    };
    let Some((eh, em)) = parse_hhmm(end) else {
        return false;
    };
    (sh, sm) <= (hour, 0) && (hour, 0) < (eh, em)
}

/// Parse the `HH:MM` prefix of a `"HH:MM"` string (the legacy peak-window
/// shape carried plain pairs; seconds were tolerated by the previous feed and
/// still parse).
fn parse_hhmm(s: &str) -> Option<(u8, u8)> {
    let (hh, mm) = s.split_once(':')?;
    Some((hh.parse().ok()?, mm.get(..2)?.parse().ok()?))
}

/// The UTC weekday name of a "YYYY-MM-DD" date, lowercase like the feed's
/// `days` sets spell them. `None` when the date does not parse — the
/// constraint is then never active, so entry selection falls through to the
/// base entry like any unparseable constraint.
fn date_weekday(date: &str) -> Option<&'static str> {
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(match parsed.weekday() {
        Weekday::Mon => "monday",
        Weekday::Tue => "tuesday",
        Weekday::Wed => "wednesday",
        Weekday::Thu => "thursday",
        Weekday::Fri => "friday",
        Weekday::Sat => "saturday",
        Weekday::Sun => "sunday",
    })
}

/// An HHMM clock number (`100` = 01:00) formatted as `"HH:MM"`; `2400`
/// becomes `"24:00"`, which [`window_contains`] reads as the exclusive end of
/// the last hour. `None` when the minutes exceed 59 or the hour exceeds 24 —
/// the generator validates the same bounds, and a malformed window skips its
/// entry rather than failing the row.
fn fmt_hhmm(clock: u32) -> Option<String> {
    let (hh, mm) = (clock / 100, clock % 100);
    (hh <= 24 && mm < 60).then(|| format!("{hh:02}:{mm:02}"))
}

// ── Background thread ────────────────────────────────────────────────────────

/// Events emitted by the background pricing worker.
pub(crate) enum PricingEvent {
    /// A fresh or cached table is available.
    Loaded(Box<PriceTable>),
    /// A fetch failed and no cache was available. The app flags the cost lens,
    /// which reads `rates unavailable` instead of `rates loading`.
    Failed,
}

/// Spawn the pricing worker. On start it cold-loads the disk cache (so cost
/// renders instantly and offline once primed), then fetches the live feed once
/// the cache has aged past the cadence and loops on it — the 24h table survives a
/// relaunch instead of being re-downloaded; a `()` on `refresh_rx` triggers an
/// immediate refetch. Exits when the refresh channel disconnects (TUI shutdown).
///
/// Mirrors `status::spawn`: a plain `std::thread`, a ureq agent with short
/// timeouts, and the cache path resolved on the calling thread before detaching
/// (so the worker never re-resolves `home_dir()`, which would race a test's
/// `HOME_OVERRIDE`).
pub(crate) fn spawn(tx: Sender<PricingEvent>, refresh_rx: Receiver<()>) {
    let Some(cache_file) = cache_path() else {
        return;
    };
    std::thread::spawn(move || {
        // Cold-fill from cache first so the first paint can price immediately.
        let mut cached_at_ms = None;
        if let Some(table) = load_cache(&cache_file) {
            cached_at_ms = Some(table.fetched_at_ms);
            let _ = tx.send(PricingEvent::Loaded(Box::new(table)));
        }

        let first = first_delay(cached_at_ms, now_ms(), REFRESH_INTERVAL);
        let mut stale_cleaned = false;
        run_polling_loop(&refresh_rx, first, REFRESH_INTERVAL, || {
            run_fetch(&tx, &cache_file, &mut stale_cleaned)
        });
    });
}

/// One fetch attempt. On success: distill, fold into the snapshot history,
/// cache, send `Loaded`. On failure: fall back to the cache when one exists
/// (`Loaded`); only when nothing is cached do we surface `Failed`.
fn run_fetch(tx: &Sender<PricingEvent>, cache_file: &Path, stale_cleaned: &mut bool) {
    match fetch_models() {
        Ok(models) => {
            let history = load_cache(cache_file)
                .map(|t| t.history)
                .unwrap_or_default();
            let table = PriceTable::capture(models, today_date(), now_ms(), history);
            save_cache(cache_file, &table);
            delete_stale_cache_once(cache_file, stale_cleaned);
            let _ = tx.send(PricingEvent::Loaded(Box::new(table)));
        }
        Err(_) => match load_cache(cache_file) {
            Some(table) => {
                let _ = tx.send(PricingEvent::Loaded(Box::new(table)));
            }
            None => {
                let _ = tx.send(PricingEvent::Failed);
            }
        },
    }
}

/// One-time best-effort removal of the pre-ai-pricelog cache files
/// (`price_cache.json` from the LiteLLM era, `genai_price_cache.json` from the
/// genai-prices era), run after the first successful save of the new cache.
/// The new table never reads the old files, so this is pure cleanup; errors
/// (including NotFound, the expected steady state) are ignored on purpose. The
/// flag is set BEFORE the deletes so a reappearing file is never re-deleted.
fn delete_stale_cache_once(cache_file: &Path, done: &mut bool) {
    if *done {
        return;
    }
    *done = true;
    let Some(dir) = cache_file.parent() else {
        return;
    };
    for stale in ["price_cache.json", "genai_price_cache.json"] {
        let _ = std::fs::remove_file(dir.join(stale));
    }
}

/// Fetch and distill the live feed. The body is capped at [`MAX_BODY_BYTES`].
fn fetch_models() -> anyhow::Result<Vec<PricedModel>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RECV_TIMEOUT))
        .build()
        .into();

    let reader = agent
        .get(FEED_URL)
        .header("User-Agent", "clauth-pricing")
        .call()
        .map_err(anyhow::Error::from)?
        .into_body()
        .into_reader();
    // +1 so a body exactly at the cap still trips the over-limit check.
    let mut capped = reader.take(MAX_BODY_BYTES + 1);

    let mut bytes = Vec::new();
    capped
        .read_to_end(&mut bytes)
        .map_err(anyhow::Error::from)?;
    if bytes.len() as u64 > MAX_BODY_BYTES {
        anyhow::bail!("price feed exceeded {MAX_BODY_BYTES} byte cap");
    }
    let json = String::from_utf8(bytes).map_err(anyhow::Error::from)?;
    distill(&json)
}

// ── Distill ──────────────────────────────────────────────────────────────────

/// Parse the ai-pricelog index JSON into distilled [`PricedModel`]s.
/// Tolerant at every level: malformed sources, rows, and window entries are
/// skipped; the fetch fails only when ZERO models survive (an empty table
/// would price nothing and look like a healthy load). Only
/// [`FIRST_PARTY_PROVIDERS`] are kept — resellers are dropped here, so no
/// lookup path can land on a reseller's markup; resold model rows inside a
/// kept provider (a claude id under a non-anthropic provider) are dropped for
/// the same reason. An unknown `version` warns through [`logline!`] and parses
/// best-effort.
///
/// A row carrying `removed_at` is delisted and skipped the same way — it
/// prices nothing at any date. The index's removal convention keeps the row
/// in place with its last prices and stamps it, so the skip is what makes a
/// delisting effective in clauth (the dashscope resold deepseek/glm/kimi
/// copies carry the stamp since 2026-08-31).
///
/// Sources iterate in the file's own key order (serde_json's `preserve_order`
/// feature) — deterministic, and carrying no precedence contract: a delisted
/// copy under another source is skipped at distill, so a live first-party row
/// is never shadowed and iteration order cannot pick a price.
fn distill(json: &str) -> anyhow::Result<Vec<PricedModel>> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(anyhow::Error::from)?;
    let root = root
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("price feed root is not a JSON object"))?;
    let version = root.get("version").and_then(serde_json::Value::as_u64);
    if version != Some(INDEX_VERSION) {
        let found = version.map_or_else(|| "missing".to_owned(), |v| v.to_string());
        logline!(
            "price feed: ai-pricelog index version {found} (expected {INDEX_VERSION}): parsing best-effort"
        );
    }
    let sources = root
        .get("sources")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("price feed has no sources object"))?;

    let mut models = Vec::new();
    for (source_name, rows) in sources {
        let canonical = canonical_source(source_name);
        if !FIRST_PARTY_PROVIDERS.contains(&canonical) {
            continue;
        }
        let Some(rows) = rows.as_object() else {
            continue; // malformed source — skip, don't fail the feed
        };
        for (model_id, row) in rows {
            let Ok(row) = serde_json::from_value::<RawRow>(row.clone()) else {
                continue; // malformed row — skip, don't fail the source
            };
            // A delisted row (`removed_at` stamped) is out of the index's
            // current price set and prices nothing.
            if row.removed_at.is_some() {
                continue;
            }
            // A kept provider can still resell another vendor's models: a
            // claude id under a non-anthropic provider is a resold listing and
            // drops, case-insensitively on the id prefix. Anthropic's own rows
            // are the only legitimate claude rows.
            if canonical != "anthropic"
                && model_id
                    .get(..6)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("claude"))
            {
                continue;
            }
            if let Some(priced) = row.into_priced(model_id.clone()) {
                models.push(priced);
            }
        }
    }
    if models.is_empty() {
        anyhow::bail!("price feed distilled to zero priced models");
    }
    Ok(models)
}

/// The index's source spelling → clauth's canonical provider name; the
/// allowlist tests the canonical name. Only moonshot and xai differ today.
fn canonical_source(source: &str) -> &str {
    match source {
        "moonshot" => "moonshotai",
        "xai" => "x-ai",
        other => other,
    }
}

/// One index row. The four `_mtok` keys are USD per million tokens; missing
/// keys are `None` (→ 0.0 per token). `cache_write_1h_mtok` is deliberately
/// NOT declared (the hourly axis has no TTL data), and index-only extras
/// (`first_seen`, `timezone`, `observed_at`, …) are ignored the same way. A
/// declared field of the wrong shape fails the whole row, which the caller
/// then skips.
#[derive(Deserialize)]
struct RawRow {
    #[serde(default)]
    input_mtok: Option<f64>,
    #[serde(default)]
    output_mtok: Option<f64>,
    #[serde(default)]
    cache_read_mtok: Option<f64>,
    #[serde(default)]
    cache_write_mtok: Option<f64>,
    /// Present = the row is delisted (the index's removal convention keeps
    /// the row with its last prices and stamps it); the caller skips it, so
    /// it prices nothing at any date.
    #[serde(default)]
    removed_at: Option<String>,
    #[serde(default)]
    effective_at: Option<String>,
    #[serde(default)]
    window_rates: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    peak_windows: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    peak_input_mtok: Option<f64>,
    #[serde(default)]
    peak_output_mtok: Option<f64>,
    #[serde(default)]
    peak_cache_read_mtok: Option<f64>,
    #[serde(default)]
    peak_cache_write_mtok: Option<f64>,
}

/// The four token rates one entry can carry. A window entry holds OVERRIDE
/// rates only: a key it leaves absent inherits the row's base value at
/// distill time.
#[derive(Debug, Default, Clone, Copy)]
struct RawRateSet {
    input_mtok: Option<f64>,
    output_mtok: Option<f64>,
    cache_read_mtok: Option<f64>,
    cache_write_mtok: Option<f64>,
}

impl RawRateSet {
    /// Per-token entry; `base` fills the keys this set leaves absent. `None`
    /// values price 0.0.
    fn entry(&self, base: &Self, constraint: Option<Constraint>) -> PriceEntry {
        PriceEntry {
            input: to_per_token(self.input_mtok.or(base.input_mtok).unwrap_or(0.0)),
            output: to_per_token(self.output_mtok.or(base.output_mtok).unwrap_or(0.0)),
            cache_read: to_per_token(self.cache_read_mtok.or(base.cache_read_mtok).unwrap_or(0.0)),
            cache_write: to_per_token(
                self.cache_write_mtok
                    .or(base.cache_write_mtok)
                    .unwrap_or(0.0),
            ),
            constraint,
        }
    }
}

/// One `window_rates` entry: an HHMM `[start, end]` window, an optional
/// UTC-weekday `days` set (absent = every day), and override rate keys. The
/// `quota_multiplier` key is deliberately NOT declared: it is a consumption
/// weight, never a rate, and an entry with no rate keys of its own is skipped
/// by the caller.
#[derive(Deserialize)]
struct RawWindowEntry {
    #[serde(default)]
    window: Option<[u32; 2]>,
    #[serde(default)]
    days: Option<Vec<String>>,
    #[serde(default)]
    input_mtok: Option<f64>,
    #[serde(default)]
    output_mtok: Option<f64>,
    #[serde(default)]
    cache_read_mtok: Option<f64>,
    #[serde(default)]
    cache_write_mtok: Option<f64>,
}

impl RawWindowEntry {
    /// `None` for an entry with no rate keys of its own (a quota-only entry)
    /// or a malformed window — both are dropped, never widened into an
    /// always-active entry.
    fn to_entry(&self, base: &RawRateSet) -> Option<PriceEntry> {
        if !self.any_rate() {
            return None;
        }
        let window = match self.window {
            None => None,
            Some([start, end]) => Some((fmt_hhmm(start)?, fmt_hhmm(end)?)),
        };
        let constraint = match (window, self.days.as_deref()) {
            (Some((start, end)), Some(days)) => Some(Constraint::Days {
                days: days.to_vec(),
                start: Some(start),
                end: Some(end),
            }),
            (Some((start, end)), None) => Some(Constraint::TimeWindow { start, end }),
            (None, Some(days)) => Some(Constraint::Days {
                days: days.to_vec(),
                start: None,
                end: None,
            }),
            (None, None) => None,
        };
        Some(self.rates().entry(base, constraint))
    }

    fn any_rate(&self) -> bool {
        self.input_mtok.is_some()
            || self.output_mtok.is_some()
            || self.cache_read_mtok.is_some()
            || self.cache_write_mtok.is_some()
    }

    fn rates(&self) -> RawRateSet {
        RawRateSet {
            input_mtok: self.input_mtok,
            output_mtok: self.output_mtok,
            cache_read_mtok: self.cache_read_mtok,
            cache_write_mtok: self.cache_write_mtok,
        }
    }
}

impl RawRow {
    fn into_priced(self, id: String) -> Option<PricedModel> {
        let base = RawRateSet {
            input_mtok: self.input_mtok,
            output_mtok: self.output_mtok,
            cache_read_mtok: self.cache_read_mtok,
            cache_write_mtok: self.cache_write_mtok,
        };
        let mut prices = vec![base.entry(&RawRateSet::default(), None)];
        for raw in self.window_rates.into_iter().flatten() {
            let Ok(entry) = serde_json::from_value::<RawWindowEntry>(raw) else {
                continue; // malformed entry — skip, don't fail the row
            };
            if let Some(entry) = entry.to_entry(&base) {
                prices.push(entry);
            }
        }
        // The legacy peak shape: `peak_windows` as ["HH:MM","HH:MM"] STRING
        // pairs plus flat `peak_*` rate keys (absent peak keys inherit the
        // base, like window entries). No live row carries it; the generator
        // may regress to it.
        let peak = RawRateSet {
            input_mtok: self.peak_input_mtok,
            output_mtok: self.peak_output_mtok,
            cache_read_mtok: self.peak_cache_read_mtok,
            cache_write_mtok: self.peak_cache_write_mtok,
        };
        for raw in self.peak_windows.into_iter().flatten() {
            let Ok([start, end]) = serde_json::from_value::<[String; 2]>(raw) else {
                continue; // malformed pair — skip
            };
            prices.push(peak.entry(&base, Some(Constraint::TimeWindow { start, end })));
        }
        // A model with no input AND no output rate anywhere (per-request /
        // web-search pricing) cannot price tokens; keeping it would render a
        // $0 "priced" row instead of an unpriced dash.
        if !prices.iter().any(|e| e.input != 0.0 || e.output != 0.0) {
            return None;
        }
        Some(PricedModel {
            id,
            prices,
            effective_at: self.effective_at,
        })
    }
}

/// USD-per-million → per-token.
fn to_per_token(mtok: f64) -> f64 {
    mtok / 1e6
}

// ── Disk cache ───────────────────────────────────────────────────────────────

/// On-disk cache shape: the fetch time plus the snapshot history.
#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    fetched_at_ms: u64,
    #[serde(default)]
    history: Vec<RateSnapshot>,
}

/// `~/.clauth/ai_pricelog_price_cache.json`. Resolved ONCE at spawn time and
/// passed into the worker so the detached thread never re-resolves
/// `home_dir()` later.
fn cache_path() -> Option<PathBuf> {
    clauth_dir()
        .ok()
        .map(|d| d.join("ai_pricelog_price_cache.json"))
}

/// Synchronous one-shot read of the on-disk price cache, off the background
/// [`spawn`] channel — the CLI sessions surface needs a `PriceTable` on the main
/// thread without standing up the worker. `None` when the cache is absent or
/// unparseable; never fetches, so a cold cache simply prices nothing.
pub(crate) fn load_cached() -> Option<PriceTable> {
    load_cache(&cache_path()?)
}

/// Load the cache if it exists and parses; `None` on any miss/error (a stale or
/// reshaped cache is silently treated as no cache). A cache with an empty
/// snapshot history is also rejected — without at least one snapshot nothing
/// can resolve.
fn load_cache(path: &Path) -> Option<PriceTable> {
    let bytes = std::fs::read_to_string(path).ok()?;
    let cache: CacheFile = serde_json::from_str(&bytes).ok()?;
    let models = cache.history.last()?.models.clone();
    Some(PriceTable {
        models,
        history: cache.history,
        fetched_at_ms: cache.fetched_at_ms,
        memo: Mutex::default(),
    })
}

/// Persist the cache best-effort (atomic tmp + rename). Errors are swallowed.
fn save_cache(path: &Path, table: &PriceTable) {
    let cache = CacheFile {
        fetched_at_ms: table.fetched_at_ms,
        history: table.history.clone(),
    };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = atomic_write_600(path, json);
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "../tests/inline/pricing.rs"]
mod tests;
