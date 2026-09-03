//! Model price table — fetches per-token USD rates so the Tokens tab can show
//! API-equivalent cost (what the recorded usage *would* cost at pay-as-you-go
//! API rates; clauth users are on subscription plans, so this reads as "value
//! extracted", not a bill).
//!
//! # Source
//!
//! The ai-pricelog index (`index.json` on the `dist` branch of
//! uwuclxdy/ai-pricelog, version 4): a `sources` object mapping provider name
//! → model id → a row whose `rates` object maps a price axis to USD per
//! million tokens. clauth models four axes (`input`, `output`, `cache_read`,
//! `cache_write`); every other axis the store carries (`cache_write_1h`,
//! `image`, `audio`, `internal_reasoning`, …) has no bucket to charge to and
//! is ignored, as are `limits`, `fees` and `provenance`. A row may carry
//! `effective_at` (prices apply from that date on; earlier dates price
//! nothing) and `overrides` entries, each a `when` plus override `rates`, a
//! `quota_multiplier`, or both. A `when` takes `days` (lowercase weekday
//! names, already UTC calendar days), `window` (an `[HHMM, HHMM]` pair),
//! `min_tokens` (a token-volume threshold) and `timezone` — provenance only:
//! the store converted the schedule to UTC at build time, so re-deriving
//! `days` from the zone would shift it twice. An override axis left absent
//! inherits the row's base; later entries override earlier matching ones. A
//! row carrying `removed_at` is delisted and prices nothing.
//!
//! Whether a row is RESOLD is a per-row comparison against the catalog, not a
//! provider allowlist: the row is first-party when the model's `vendor` and
//! the provider's are the same named vendor. A provider publishing no
//! `vendor` resells everything, a model the catalog gives no vendor is
//! nobody's first-party row, and a pair the catalog does not name has no
//! maker to compare — all three are resales. Resold rows drop at distill, so
//! no lookup path can land on a reseller's markup, and a MIXED provider comes
//! out right with no special case: dashscope keeps its qwen rows and drops
//! the deepseek / glm / kimi ids it resells.
//!
//! Dated rates come from the store's history ndjson (`history.ndjson`),
//! fetched alongside the index: one JSON row per line, every row carrying
//! `observed_at` (the day the scraper saw it), optionally `effective_at` (the
//! day the price applies FROM — a retro-dated change) and `removed: true`
//! (the model delisted from that day on). A query date prices at the row with
//! the greatest `observed_at` among that `(source, model_id)` key's rows whose
//! `effective_at ?? observed_at` falls on or before the date, back to the
//! store's oldest row. A removal row winning a day's walk prices nothing for
//! that day, and a price row appended after a removal re-lives the key from
//! its own applies day. The same resold guard applies to history rows, so a
//! reseller's copy of an id can neither price it nor delist it.
//! Cross-source twin-ness resolves through the catalog's model registry
//! (canonical id → each source's spellings of it): a delisted key whose
//! canonical id a live key holds never prices any date, and a pair the
//! catalog does not name stays its own key.
//!
//! Api-alias ids (`deepseek-chat`, `deepseek-reasoner`) name a served model,
//! not a page: no index row and no history key carries them, so they resolve
//! through the catalog's alias chains (`catalog/aliases.json`) — a dated
//! chain per alias, one record per canonical model that served it. A query
//! day prices at the record whose `from <= day < to` window covers it, at
//! that canonical's own store walk for the day; a day no record covers prices
//! nothing (deepseek retired both aliases on 2026-07-24, and past that day
//! they dash). A claude session's variant spellings (`deepseek-v4-pro-thinking`)
//! carry a suffix no page id has; one trailing `-<suffix>` group from a known
//! variant list strips and retries once the ladder misses, never remapping an
//! id a row carries verbatim.
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
//! Every successful fetch appends a snapshot to the table's local snapshot
//! log (skipped when the distilled models are byte-identical to the last
//! snapshot's, capped at [`HISTORY_CAP`] snapshots). The snapshot log is the
//! offline cache only — nothing dates off it: dating is the store walk above.
//! A table holding no store history (a cache written before this split) keeps
//! the snapshot walk — newest snapshot with `captured <= date`, oldest
//! snapshot for older dates — until the next successful fetch persists the
//! store.
//!
//! # Cost basis
//!
//! Cost is computed **per model** and summed — never via a blended rate, since
//! family rates differ up to 10× (Opus $5/$25 vs Haiku $1/$5 per 1M). It always
//! counts cache tokens (they cost real money on the API), independent of the
//! Tokens tab's `count_cache` display toggle. Models with no matching rate
//! (unknown / unpriced providers) contribute nothing and are surfaced as such.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// Live price feed: the ai-pricelog index, the newest priced row per
/// `(source, model_id)` key, from the published `dist` branch.
const INDEX_URL: &str = "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/dist/index.json";

/// The store's price history, one JSON row per line, merged across sources.
/// Fetched on the same cadence as the index; every file below must arrive for
/// a fetch to succeed, so a fresh table never mixes a new index with stale
/// dating or a stale resold guard.
const HISTORY_URL: &str =
    "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/dist/history.ndjson";

/// Catalog, model registry (`{version, models}`): every canonical model's
/// `vendor` — the MAKER half of the resold comparison — and its `sources`
/// map, each source's spellings of it. That map is also the cross-source
/// twin-ness basis of the store walk.
const CATALOG_MODELS_URL: &str =
    "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/dist/catalog/models.json";

/// Catalog, provider registry (`{version, providers}`): each source's
/// `vendor` — the PROVIDER half of the resold comparison, absent on a
/// reseller — beside its display name and `kind`.
const CATALOG_PROVIDERS_URL: &str =
    "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/dist/catalog/providers.json";

/// Catalog, api-alias chains (`{version, aliases}`). No index row and no
/// history key carries an alias id, so this file is the only place the store
/// says what they served.
const CATALOG_ALIASES_URL: &str =
    "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/dist/catalog/aliases.json";

/// Background refresh cadence. Prices move rarely, so this is deliberately slow;
/// a manual refresh signal short-circuits the wait.
const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// HTTP connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// HTTP response-receive timeout.
const RECV_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard cap on a response body. Measured against the store 2026-09-03: the
/// index is ~336 KiB, the history ~509 KiB (largest single-day batch: 608
/// rows on 2026-08-29), and the three catalog files ~164 KiB (models),
/// ~12 KiB (aliases) and ~3 KiB (providers). 8 MiB is generous headroom while
/// still bounding a hostile / runaway response.
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// Snapshot history cap: the newest 180 fetches survive, older ones drop.
const HISTORY_CAP: usize = 180;

/// The ai-pricelog index version this table parses. An unknown version warns
/// through [`logline!`] and parses best-effort.
const INDEX_VERSION: u64 = 4;

/// Variant-suffix spellings a claude session reports that no price page id
/// carries (`deepseek-v4-pro-thinking`): one trailing `-<suffix>` group
/// stripped and retried after the ladder misses. Slash-prefixed reseller
/// spellings (`qwen/qwen3.6-27b`) need no entry — the ladder's provider-strip
/// rung already covers that class.
const VARIANT_SUFFIXES: &[&str] = &["thinking"];

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

/// The catalog's canonical map (`catalog/models.json`): source → model id →
/// the canonical id every source's spelling of that model shares. Only
/// catalog-named pairs are held; a lookup miss is the identity fallback (see
/// [`PriceTable::canonical_of`]).
pub(crate) type CanonicalMap = HashMap<String, HashMap<String, String>>;

/// One `(source, model_id)` key of the store's history: the canonical source
/// name plus the model id (the id IS the match, as in the index) plus the
/// key's rows in store append order. Two kept sources can carry the same id;
/// [`PriceTable::dated_models`] materializes live keys ahead of delisted ones
/// and drops a delisted key whose canonical id a live key holds, and within
/// each group the earlier key in [`PriceTable::store`] order wins the ladder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoreKey {
    /// Canonical provider name. Empty on a cache written before this field
    /// existed; the canonical lookup then misses and falls back to the raw
    /// id, the pre-map behavior.
    #[serde(default)]
    source: String,
    id: String,
    rows: Vec<StoreRow>,
}

/// One history row. `applies` is the day the row starts applying — its
/// `effective_at` when present (a retro-dated price), else its `observed_at`.
/// A `removed` row is a delisting: it prices nothing itself, and it wins the
/// walk for the days it is newest for. `model` is absent on removal rows and
/// on rows that carry no per-token rates, and a winning row without one
/// prices nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoreRow {
    observed: String,
    applies: String,
    #[serde(default)]
    removed: bool,
    #[serde(default)]
    model: Option<PricedModel>,
}

/// One dated record of an alias chain: the canonical id the alias served from
/// `from` (inclusive) to `to` (exclusive), either bound null = open. Most
/// chains hold a single `(null, null)` record — an alias that has always
/// pointed at one still-live canonical (`grok-4.5-latest` → `grok-4.5`). The
/// store's `citation` is provenance for a human, not a rate, and is not
/// distilled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AliasSpan {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    canonical: String,
}

impl AliasSpan {
    /// Whether this record is the alias's answer on `date`: `from <= date < to`
    /// with either bound open when null.
    fn covers(&self, date: &str) -> bool {
        self.from.as_deref().is_none_or(|from| from <= date)
            && self.to.as_deref().is_none_or(|to| date < to)
    }
}

/// One alias id and its dated chain, in file order. A chain need not be
/// contiguous and need not reach today: an alias past its last record is
/// retired, and the days after it price nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AliasKey {
    id: String,
    spans: Vec<AliasSpan>,
}

/// Per-hour token buckets behind [`PriceTable::cost_day`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HourTokens {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_create: u64,
}

/// Resolved price table: the store's distilled history (the dating source for
/// every day), the newest snapshot's models plus the local snapshot log (the
/// offline cache), the catalog's two distilled halves (alias chains + the
/// canonical map), and the wall-clock time the feed was fetched (for a
/// freshness badge).
#[derive(Debug)]
pub(crate) struct PriceTable {
    /// Latest snapshot's models — the head of the snapshot log.
    models: Vec<PricedModel>,
    /// Oldest-first local snapshot log; [`PriceTable::models_for`] walks it
    /// only for a table holding no store history.
    history: Vec<RateSnapshot>,
    /// The store's distilled history rows, first-seen key order — the dating
    /// source for every day, today included. Empty while no store history has
    /// been fetched or cached (a pre-store cache).
    store: Vec<StoreKey>,
    /// The store's distilled alias chains (`catalog/aliases.json`). Empty until
    /// a fetch or cache brings them (a pre-aliases cache loads empty and
    /// upgrades on the next successful fetch), and then only reached once the
    /// ladder and the variant strip have both missed.
    aliases: Vec<AliasKey>,
    /// The catalog's canonical `(source, id)` → canonical-id map
    /// (`catalog/models.json`): the twin-ness basis of
    /// [`PriceTable::dated_models`]. Empty
    /// until a fetch or cache brings it (a pre-map cache loads empty and
    /// upgrades on the next successful fetch); an empty map is the identity
    /// behavior — every key canonicalizes to its raw id.
    canonical: CanonicalMap,
    pub(crate) fetched_at_ms: u64,
    /// Memoized match walks (see [`Memo`]). A table is immutable once built, so
    /// a remembered index cannot go stale.
    memo: Mutex<Memo>,
}

/// Match walks and dated model sets already computed, `date →` both halves
/// ([`DatedSet`]). A table is immutable once built, so a remembered set or
/// index cannot go stale.
///
/// A walk strips and retries the id and scans every model's id once per
/// candidate form, and the cost lens asks for the same `(id, date)` at all 24
/// hours of a day on every frame — the weekly lens measured `days × 24` walks
/// per model per frame on 2026-08-20. The pick is hour-independent (the walk
/// chooses the MODEL, an entry's constraint chooses that model's RATE), so one
/// walk answers every hour, and the memo carries it across frames.
#[derive(Debug, Default)]
struct Memo {
    by_date: HashMap<String, DatedSet>,
    /// Cold walks performed. The memo's only observable, so the tests that pin
    /// "once per (model, date)" have something to count.
    #[cfg(test)]
    walks: usize,
}

/// One date's materialized model set plus the id walks already done against
/// it. `by_id` indexes only ever resolve against `models` of the same date,
/// so the pair cannot be crossed up by a caller.
#[derive(Debug, Default)]
struct DatedSet {
    models: Arc<[PricedModel]>,
    by_id: HashMap<String, Option<usize>>,
}

impl PriceTable {
    /// Fold a successful fetch into a table: stamp `fetched_at_ms`, append a
    /// snapshot dated `captured` ONLY when the distilled models differ from the
    /// last snapshot's (serialize-and-compare — a byte-identical refetch does
    /// not grow the history), and cap the history at [`HISTORY_CAP`], dropping
    /// the oldest snapshots. `store` is the fetch's complete distilled history
    /// — the file is cumulative — and replaces any cached store wholesale, as
    /// `aliases` and `canonical` do the catalog's two halves.
    pub(crate) fn capture(
        models: Vec<PricedModel>,
        store: Vec<StoreKey>,
        aliases: Vec<AliasKey>,
        canonical: CanonicalMap,
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
            store,
            aliases,
            canonical,
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
    /// Two stages follow a ladder miss, in [`resolve_index`]: the variant
    /// strip (`deepseek-v4-pro-thinking` → `deepseek-v4-pro`, one trailing
    /// `-<suffix>` group from [`VARIANT_SUFFIXES`], case-insensitively), then
    /// the alias table ([`alias_for`]: `deepseek-chat` on a day inside one of
    /// its records prices that record's canonical id, which itself re-enters
    /// the full ladder). An id a row carries verbatim is never remapped by
    /// either — both run only on a miss.
    ///
    /// Steps 1-2 and the retry ladder are [`ladder_index`]; the whole walk is
    /// memoized per `(id, date)`; step 3 is [`entry_rate`], the only half the
    /// hour reaches.
    ///
    /// Rates come from the models applicable to `date` (see
    /// [`PriceTable::dated_models`]: the store walk, or the snapshot walk for
    /// a table holding no store history); `None` when no model matches, and
    /// `None` for a matched row whose `effective_at` is after `date`.
    pub(crate) fn rate_at(&self, model: &str, date: &str, hour: u8) -> Option<ModelRate> {
        let (models, idx) = self.matched(model, date)?;
        entry_rate(&models[idx], date, hour)
    }

    /// The [`PricedModel`] that prices `model` on `date`, as an index into the
    /// date's model set returned ALONGSIDE that set — the set is owned by
    /// [`Memo`], so the caller needs the clone to hold the reference. Answered
    /// from the memo when the pair has been asked before; the memoized index
    /// only ever resolves against the same date's set, so the two cannot be
    /// crossed up by a caller. A poisoned memo walks rather than failing a
    /// price — it holds derived state and nothing else.
    fn matched(&self, model: &str, date: &str) -> Option<(Arc<[PricedModel]>, usize)> {
        let models = self.dated_models(date)?;
        let Ok(mut memo) = self.memo.lock() else {
            let idx = resolve_index(&models, model, date, &self.aliases)?;
            return Some((models, idx));
        };
        let found = {
            let set = memo.by_date.entry(date.to_owned()).or_default();
            if let Some(hit) = set.by_id.get(model) {
                return Some((models, (*hit)?));
            }
            let found = resolve_index(&models, model, date, &self.aliases);
            set.by_id.insert(model.to_owned(), found);
            found
        };
        #[cfg(test)]
        {
            memo.walks += 1;
        }
        Some((models, found?))
    }

    /// Cold match walks performed so far — what pins the memo, since a warm
    /// answer is otherwise indistinguishable from a repeated walk.
    #[cfg(test)]
    fn walks(&self) -> usize {
        self.memo.lock().expect("memo lock").walks
    }

    /// The models applicable to `date`: the store walk ([`StoreKey::row_for`],
    /// one winning row per key, first-seen key order) when store history is
    /// held — for every date, today included — else the snapshot walk
    /// ([`models_for`]) for a pre-store cache. Materialized once per date and
    /// shared through [`Memo`]; a poisoned memo recomputes rather than failing
    /// a price.
    fn dated_models(&self, date: &str) -> Option<Arc<[PricedModel]>> {
        if let Ok(memo) = self.memo.lock()
            && let Some(set) = memo.by_date.get(date)
        {
            return Some(Arc::clone(&set.models));
        }
        let models: Arc<[PricedModel]> = if self.store.is_empty() {
            self.models_for(date)?.into()
        } else {
            // Twin-ness is canonical (the catalog's model registry), so two
            // kept sources sharing a model resolve as one however each spells
            // it. No live pair reaches this today: the resold guard leaves
            // 256 keys with zero duplicate ids and zero canonical collisions
            // (measured 2026-09-03), since a model's maker is one provider and
            // every other carrier of it is a resale dropped at distill. What
            // the partition still covers is a CACHED store from before that
            // guard, and the day two vendor-carrying providers publish one
            // canonical between them. LIVE keys materialize first, so the
            // ladder's first match lands on a first-party row rather than a
            // markup; a DELISTED key whose canonical a live key holds never
            // prices any date, its own pre-removal days included; two live
            // keys on one canonical keep first-seen order, unchanged.
            let (live, delisted): (Vec<_>, Vec<_>) =
                self.store.iter().partition(|key| !key.delisted());
            let resold = |key: &StoreKey| {
                live.iter().any(|live_key| {
                    self.canonical_of(live_key)
                        .eq_ignore_ascii_case(self.canonical_of(key))
                })
            };
            live.iter()
                .chain(delisted.iter().filter(|key| !resold(key)))
                .filter_map(|key| key.row_for(date))
                .cloned()
                .collect::<Vec<_>>()
                .into()
        };
        if let Ok(mut memo) = self.memo.lock() {
            memo.by_date
                .entry(date.to_owned())
                .or_insert_with(|| DatedSet {
                    models: Arc::clone(&models),
                    by_id: HashMap::new(),
                });
        }
        Some(models)
    }

    /// The key's canonical id per the catalog's model registry: the
    /// `(source, id)` pair's mapped canonical id — the id every source's
    /// spelling of the model shares. A pair the catalog does not name is its
    /// own canonical (the identity fallback).
    fn canonical_of<'a>(&'a self, key: &'a StoreKey) -> &'a str {
        self.canonical
            .get(&key.source)
            .and_then(|ids| ids.get(&key.id))
            .map_or(key.id.as_str(), String::as_str)
    }

    /// The snapshot walk: the newest snapshot with `captured <= date` (served
    /// straight from `models`, the newest snapshot's working set); a date
    /// older than every snapshot uses the oldest one. Only reached for a table
    /// holding no store history.
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
        let (models, idx) = self.matched(model, date)?;
        let priced = &models[idx];
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

impl StoreKey {
    /// The row that prices `date`: the greatest `observed_at` among the key's
    /// rows whose `applies` day is on or before `date` — a tie on `observed_at`
    /// goes to the later row in store append order, the newer write. The
    /// terminator is per-date: a REMOVAL row winning the walk prices nothing
    /// for that day (whatever prices the row still carries are never read),
    /// and a price row appended after a removal re-lives the key from its own
    /// `applies` day — the store's index un-stamps the same key when its
    /// newest row is priced again. `None` = the day prices nothing (no
    /// candidate, the winner is a removal, or the winner carries no per-token
    /// rates).
    fn row_for(&self, date: &str) -> Option<&PricedModel> {
        let winner = self
            .rows
            .iter()
            .filter(|r| r.applies.as_str() <= date)
            .max_by(|a, b| a.observed.cmp(&b.observed))?;
        if winner.removed {
            return None;
        }
        winner.model.as_ref()
    }

    /// Whether the store's own index regeneration stamps this key `removed_at`
    /// (its newest row is a removal): the store's evidence the key was
    /// reselling another vendor's id.
    fn delisted(&self) -> bool {
        self.rows.last().is_some_and(|r| r.removed)
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

/// The full match walk for one date's model set: [`ladder_index`] first (so an
/// id a row carries verbatim is never remapped), then the variant-suffix strip
/// ([`variant_base`]), then the store's alias table ([`alias_for`]). Each
/// stage's form re-enters the full ladder, so a stage prices only what a row
/// names, and every stage missing prices nothing.
fn resolve_index(
    models: &[PricedModel],
    id: &str,
    date: &str,
    aliases: &[AliasKey],
) -> Option<usize> {
    if let Some(i) = ladder_index(models, id) {
        return Some(i);
    }
    // The ladder's primary form (bracket-stripped) is the id the two
    // id-shape stages reason about.
    let form = strip_bracket_suffix(id);
    if let Some(base) = variant_base(form)
        && let Some(i) = ladder_index(models, base)
    {
        return Some(i);
    }
    let canonical = alias_for(aliases, form, date)?;
    ladder_index(models, canonical)
}

/// The canonical id an alias served on `date`: its chain's record covering the
/// day. `None` when the id is no alias or no record covers it — an alias past
/// its last record is retired, and the days after it price nothing.
fn alias_for<'a>(aliases: &'a [AliasKey], id: &str, date: &str) -> Option<&'a str> {
    let key = aliases.iter().find(|k| k.id.eq_ignore_ascii_case(id))?;
    key.spans
        .iter()
        .find(|span| span.covers(date))
        .map(|span| span.canonical.as_str())
}

/// The id with one variant-suffix group stripped: `deepseek-v4-pro-thinking`
/// → `deepseek-v4-pro`. Only the suffixes [`VARIANT_SUFFIXES`] names strip — a
/// loose `-<segment>` strip would walk past a real variant name into the
/// family base id and price a different model than the id names, the same
/// reason [`strip_date_stamp`] pins its digit width.
fn variant_base(id: &str) -> Option<&str> {
    VARIANT_SUFFIXES
        .iter()
        .find_map(|suffix| strip_variant_suffix(id, suffix))
}

/// Strip one trailing `-<suffix>` variant marker, case-insensitively.
fn strip_variant_suffix<'a>(id: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = id.len().checked_sub(suffix.len() + 1)?;
    // An ASCII `-` at `cut` puts both sides on char boundaries, so the slices
    // below cannot split a multi-byte character.
    if id.as_bytes().get(cut) != Some(&b'-') {
        return None;
    }
    if !id.get(cut + 1..)?.eq_ignore_ascii_case(suffix) {
        return None;
    }
    id.get(..cut)
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

/// Parse the `HH:MM` prefix of a `"HH:MM"` string. [`fmt_hhmm`] is the only
/// producer a fetch reaches, and it emits exactly `HH:MM`; the trailing part
/// is dropped rather than rejected so a [`Constraint`] deserialized from a
/// cache written against an older feed (`"00:30:00Z"`) still selects.
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
    match fetch_table() {
        Ok(feed) => {
            let history = load_cache(cache_file)
                .map(|t| t.history)
                .unwrap_or_default();
            let table = PriceTable::capture(
                feed.models,
                feed.store,
                feed.aliases,
                feed.canonical,
                today_date(),
                now_ms(),
                history,
            );
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

/// One-time best-effort removal of every superseded cache file
/// (`price_cache.json` from the LiteLLM era, `genai_price_cache.json` from the
/// genai-prices era, `ai_pricelog_price_cache.json` from the v3-feed era), run
/// after the first successful save of the new cache. The new table never reads
/// the old files, so this is pure cleanup; errors (including NotFound, the
/// expected steady state) are ignored on purpose. The flag is set BEFORE the
/// deletes so a reappearing file is never re-deleted.
fn delete_stale_cache_once(cache_file: &Path, done: &mut bool) {
    if *done {
        return;
    }
    *done = true;
    let Some(dir) = cache_file.parent() else {
        return;
    };
    for stale in [
        "price_cache.json",
        "genai_price_cache.json",
        "ai_pricelog_price_cache.json",
    ] {
        let _ = std::fs::remove_file(dir.join(stale));
    }
}

/// One successful fetch, distilled: the index's priced models (folded into the
/// snapshot log), the store's dated history keys, and the catalog's two halves
/// (alias chains + canonical map).
struct DistilledFeed {
    models: Vec<PricedModel>,
    store: Vec<StoreKey>,
    aliases: Vec<AliasKey>,
    canonical: CanonicalMap,
}

/// Fetch and distill the live feed over the network.
fn fetch_table() -> anyhow::Result<DistilledFeed> {
    fetch_table_with(fetch_body)
}

/// The feed composition, over any `fetch`: the catalog's three files (the
/// provider and model registries, then the alias chains), then the index (the
/// current price set, folded into the snapshot log) and the history ndjson
/// (the dating source). Any one failing fails the whole attempt, so the cache
/// fallback serves a coherent table rather than a mix of halves — the seam is
/// what lets a test hold that rule against each of the five files in turn,
/// which a hardcoded [`fetch_body`] gave no way to do.
///
/// The two registries distill FIRST because they build the [`FirstParty`]
/// guard, and the index and the history must be filtered by the same one: a
/// table mixing a filtered index with an unfiltered history would price an id
/// off a reseller's row on the dates the index does not cover.
fn fetch_table_with(
    fetch: impl Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<DistilledFeed> {
    let provider_vendors = distill_providers(&fetch(CATALOG_PROVIDERS_URL)?)?;
    let (canonical, first_party) = distill_catalog(&fetch(CATALOG_MODELS_URL)?, &provider_vendors)?;
    let aliases = distill_aliases(&fetch(CATALOG_ALIASES_URL)?)?;
    let models = distill(&fetch(INDEX_URL)?, &first_party)?;
    let store = distill_history(&fetch(HISTORY_URL)?, &first_party)?;
    Ok(DistilledFeed {
        models,
        store,
        aliases,
        canonical,
    })
}

/// GET one feed URL as text, capped at [`MAX_BODY_BYTES`].
fn fetch_body(url: &str) -> anyhow::Result<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RECV_TIMEOUT))
        .build()
        .into();

    let reader = agent
        .get(url)
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
        anyhow::bail!("price feed exceeded {MAX_BODY_BYTES} byte cap: {url}");
    }
    String::from_utf8(bytes).map_err(anyhow::Error::from)
}

// ── The resold guard ─────────────────────────────────────────────────────────

/// Provider name → the vendor it MAKES models for. A provider publishing no
/// vendor is absent from the map rather than mapped to a null, so "a provider
/// with no vendor resells everything" is the map's own miss and no comparison
/// can read two absent vendors as a match.
type ProviderVendors = HashMap<String, String>;

/// The `(source, model_id)` rows the catalog says a provider serves
/// FIRST-PARTY: the model's `vendor` and the provider's are the same named
/// vendor (ai-pricelog plan decision 29). Everything else is a resale — a
/// provider with no vendor, a model the catalog gives no vendor, and a pair
/// the catalog does not name at all, which has no maker to compare.
///
/// Both sides come from one fetch's two registries together, so the guard can
/// never compare a model against a provider from a different fetch. A MIXED
/// provider needs no special case: it keeps the rows it makes and drops the
/// ones it resells.
#[derive(Debug, Default)]
struct FirstParty(HashMap<String, HashSet<String>>);

impl FirstParty {
    /// Whether `source`'s row for `model_id` is a resale — the guard both
    /// distills apply per ROW.
    fn resells(&self, source: &str, model_id: &str) -> bool {
        !self.0.get(source).is_some_and(|ids| ids.contains(model_id))
    }
}

/// The ids one catalog `sources` value names. v4 spells every value as a LIST
/// of that source's spellings of the model; a bare string is read as a
/// one-element list so a hand-written catalog still resolves. Exactly one arm
/// yields — `as_str` misses an array and `as_array` misses a string — and any
/// other shape yields nothing, skipping the pair.
fn source_ids(value: &serde_json::Value) -> impl Iterator<Item = &str> {
    let single = value.as_str();
    let list = value.as_array().into_iter().flatten();
    single
        .into_iter()
        .chain(list.filter_map(serde_json::Value::as_str))
}

/// Distill the catalog's provider registry into [`ProviderVendors`]. An entry
/// with no `vendor` string is left out — it resells everything. Fails when
/// the `providers` object is missing or names no vendor at all: a registry
/// where every provider resells would drop every row downstream, and failing
/// here names the broken file instead.
fn distill_providers(json: &str) -> anyhow::Result<ProviderVendors> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(anyhow::Error::from)?;
    let providers = root
        .get("providers")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("providers feed has no providers object"))?;
    let vendors: ProviderVendors = providers
        .iter()
        .filter_map(|(source, entry)| {
            let vendor = entry.get("vendor")?.as_str()?;
            Some((source.clone(), vendor.to_owned()))
        })
        .collect();
    if vendors.is_empty() {
        anyhow::bail!("providers feed names no provider vendor");
    }
    Ok(vendors)
}

/// Distill the catalog's model registry against the provider registry into
/// the canonical `(source, id)` → canonical-id map (the twin-ness basis of
/// the store walk) plus the [`FirstParty`] guard. Tolerant like the other
/// distills: an entry with no `sources` object skips, and a `sources` value
/// of neither list nor string shape contributes nothing. Fails when the
/// canonical map distills empty (the store's v4 catalog names 724 models over
/// 1106 pairs) — an empty map would leave every cross-source twin raw AND
/// resell every row, while looking like a healthy load.
fn distill_catalog(
    json: &str,
    provider_vendors: &ProviderVendors,
) -> anyhow::Result<(CanonicalMap, FirstParty)> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(anyhow::Error::from)?;
    let models = root
        .get("models")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("models feed has no models object"))?;
    let mut canonical = CanonicalMap::new();
    let mut first_party = FirstParty::default();
    for (canonical_id, entry) in models {
        let vendor = entry.get("vendor").and_then(serde_json::Value::as_str);
        let Some(sources) = entry.get("sources").and_then(serde_json::Value::as_object) else {
            continue; // malformed entry — skip, don't fail the feed
        };
        for (source, ids) in sources {
            // The resold comparison. `vendor` is `None` for a model the
            // catalog cannot name a maker for, and the lookup misses for a
            // provider publishing none: neither can equal a named vendor, so
            // both fall out as resales without a null ever comparing equal.
            let first_party_here = provider_vendors
                .get(source)
                .is_some_and(|provider_vendor| vendor == Some(provider_vendor.as_str()));
            for id in source_ids(ids) {
                canonical
                    .entry(source.clone())
                    .or_default()
                    .insert(id.to_owned(), canonical_id.clone());
                if first_party_here {
                    first_party
                        .0
                        .entry(source.clone())
                        .or_default()
                        .insert(id.to_owned());
                }
            }
        }
    }
    if canonical.is_empty() {
        anyhow::bail!("models feed distilled to zero canonical pairs");
    }
    Ok((canonical, first_party))
}

// ── Distill ──────────────────────────────────────────────────────────────────

/// Parse the ai-pricelog index JSON into distilled [`PricedModel`]s.
/// Tolerant at every level: malformed sources, rows, and override entries are
/// skipped; the fetch fails only when ZERO models survive (an empty table
/// would price nothing and look like a healthy load). Resold rows are dropped
/// per [`FirstParty`], so no lookup path can land on a reseller's markup. An
/// unknown `version` warns through [`logline!`] and parses best-effort.
///
/// A row carrying `removed_at` is delisted and skipped the same way — it
/// prices nothing at any date. The index's removal convention keeps the row
/// in place with its last prices and stamps it, so the skip is what makes a
/// delisting effective in clauth.
///
/// Sources iterate in the file's own key order (serde_json's `preserve_order`
/// feature) — deterministic, and carrying no precedence contract: a resold or
/// delisted copy under another source is skipped at distill, so a live
/// first-party row is never shadowed and iteration order cannot pick a price.
fn distill(json: &str, first_party: &FirstParty) -> anyhow::Result<Vec<PricedModel>> {
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
        let Some(rows) = rows.as_object() else {
            continue; // malformed source — skip, don't fail the feed
        };
        for (model_id, row) in rows {
            // The resold guard: the catalog decides per ROW whether this
            // provider makes the model or resells another vendor's.
            if first_party.resells(source_name, model_id) {
                continue;
            }
            let Ok(row) = serde_json::from_value::<RawRow>(row.clone()) else {
                continue; // malformed row — skip, don't fail the source
            };
            // A delisted row (`removed_at` stamped) is out of the index's
            // current price set and prices nothing.
            if row.removed_at.is_some() {
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

/// Distill the store's history ndjson into per-key dated rows ([`StoreKey`]).
/// Line-tolerant like the index distill: a malformed line, or a line with no
/// `observed_at`, skips. The same [`FirstParty`] guard applies, so a
/// reseller's rows for an id never enter the walk: they can neither price the
/// id nor delist it (together's 2026-08-28 removal row for deepseek-v4-pro
/// cannot terminate deepseek's own key). A `removed: true` row — the
/// history's delisting spelling, where the index stamps `removed_at` — is
/// kept as a terminator although it carries no distilled prices; a price row
/// that distills to nothing is kept as an unpriced row, since the index
/// delists a key whose newest row has no per-token rates and the walk agrees
/// for that day. Fails when zero keys survive (nothing could ever resolve).
fn distill_history(ndjson: &str, first_party: &FirstParty) -> anyhow::Result<Vec<StoreKey>> {
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut keys: Vec<StoreKey> = Vec::new();
    for line in ndjson.lines() {
        let Ok(raw) = serde_json::from_str::<RawHistoryRow>(line) else {
            continue; // malformed line — skip, don't fail the feed
        };
        if first_party.resells(&raw.source, &raw.model_id) {
            continue;
        }
        let removed = raw.removed == Some(true);
        let applies = raw
            .row
            .effective_at
            .clone()
            .unwrap_or_else(|| raw.observed_at.clone());
        let key = (raw.source.clone(), raw.model_id.clone());
        let id = key.1.clone();
        let row = StoreRow {
            observed: raw.observed_at.clone(),
            applies,
            removed,
            model: if removed {
                None
            } else {
                raw.row.into_priced(raw.model_id)
            },
        };
        let slot = match index.get(&key) {
            Some(&slot) => slot,
            None => {
                keys.push(StoreKey {
                    source: key.0.clone(),
                    id,
                    rows: Vec::new(),
                });
                index.insert(key, keys.len() - 1);
                keys.len() - 1
            }
        };
        keys[slot].rows.push(row);
    }
    if keys.is_empty() {
        anyhow::bail!("price history distilled to zero keys");
    }
    Ok(keys)
}

/// Distill the catalog's api-alias chains. Tolerant like the other distills:
/// a record without a `canonical` skips, an alias whose records all skip
/// drops, a non-array chain drops. Fails when zero aliases survive (the
/// store's v4 catalog carries 44) — an empty table would dash every alias id
/// while looking like a healthy load.
fn distill_aliases(json: &str) -> anyhow::Result<Vec<AliasKey>> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(anyhow::Error::from)?;
    let aliases = root
        .get("aliases")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("aliases feed has no aliases object"))?;
    let mut keys = Vec::new();
    for (id, records) in aliases {
        let Some(records) = records.as_array() else {
            continue; // malformed chain — skip, don't fail the feed
        };
        let spans: Vec<AliasSpan> = records
            .iter()
            .filter_map(|r| serde_json::from_value::<AliasSpan>(r.clone()).ok())
            .collect();
        if !spans.is_empty() {
            keys.push(AliasKey {
                id: id.clone(),
                spans,
            });
        }
    }
    if keys.is_empty() {
        anyhow::bail!("aliases feed distilled to zero aliases");
    }
    Ok(keys)
}

/// The rate axes clauth charges tokens to, in USD per million; a missing axis
/// is `None` (→ 0.0 per token). Every other axis the store's `rates` object
/// carries (`cache_write_1h`, `image`, `audio`, `internal_reasoning`, …) is
/// deliberately NOT declared: clauth counts no such tokens, so there is no
/// bucket to charge them to. `cache_write` is the 5-minute-TTL creation rate;
/// the 1-hour axis has no TTL data to select it by.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
struct RawRates {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

impl RawRates {
    /// Per-token entry; `base` fills the axes this set leaves absent — an
    /// override carries only what it changes. `None` values price 0.0.
    fn entry(&self, base: &Self, constraint: Option<Constraint>) -> PriceEntry {
        PriceEntry {
            input: to_per_token(self.input.or(base.input).unwrap_or(0.0)),
            output: to_per_token(self.output.or(base.output).unwrap_or(0.0)),
            cache_read: to_per_token(self.cache_read.or(base.cache_read).unwrap_or(0.0)),
            cache_write: to_per_token(self.cache_write.or(base.cache_write).unwrap_or(0.0)),
            constraint,
        }
    }
}

/// When one `overrides` entry applies. `timezone` is deliberately NOT
/// declared: ai-pricelog converts every schedule to UTC calendar days at
/// build time, so the zone is provenance for a human and re-deriving `days`
/// or `window` from it would shift an already-shifted schedule twice.
#[derive(Deserialize)]
struct RawWhen {
    #[serde(default)]
    days: Option<Vec<String>>,
    #[serde(default)]
    window: Option<[u32; 2]>,
    /// A token-VOLUME threshold: the entry's rates apply to a request above
    /// it. clauth prices per hour and has no volume dimension, so
    /// [`RawOverride::to_entry`] drops such an entry rather than letting a
    /// tier rate price every request.
    #[serde(default)]
    min_tokens: Option<u64>,
}

/// One `overrides` entry: a `when` plus override `rates`, a
/// `quota_multiplier`, or both. `quota_multiplier` is deliberately NOT
/// declared — it is a consumption weight, never a rate — so an entry with no
/// `rates` of its own carries nothing clauth can price.
#[derive(Deserialize)]
struct RawOverride {
    #[serde(default)]
    when: Option<RawWhen>,
    #[serde(default)]
    rates: Option<RawRates>,
}

impl RawOverride {
    /// `None` for an entry clauth cannot express as a time-of-day rate: one
    /// with no `rates` of its own (a quota-only entry), one whose `when`
    /// names a `min_tokens` volume tier, or one whose window is malformed.
    /// All three are dropped, never widened into an always-active entry that
    /// would price every hour and every request size at that entry's rate.
    fn to_entry(&self, base: &RawRates) -> Option<PriceEntry> {
        let rates = self.rates.as_ref()?;
        let when = self.when.as_ref();
        if when.is_some_and(|when| when.min_tokens.is_some()) {
            return None;
        }
        let window = match when.and_then(|when| when.window) {
            None => None,
            Some([start, end]) => Some((fmt_hhmm(start)?, fmt_hhmm(end)?)),
        };
        let constraint = match (window, when.and_then(|when| when.days.as_deref())) {
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
        Some(rates.entry(base, constraint))
    }
}

/// One index or history row: the base [`RawRates`], the override entries, and
/// the two date stamps clauth reads. A v4 row's remaining keys (`schema`,
/// `source`, `model_id`, `observed_at`, `first_seen`, `provenance`, `fees`,
/// `limits`, `currency`) are not declared and are ignored — clauth models no
/// context limit and no per-request fee. `currency` names what the SOURCE
/// quoted, never what the row holds: ai-pricelog multiplies every rate and
/// every override rate by the fx factor at build time and records it as
/// `provenance.fx_rate`, so `rates` is USD whatever `currency` says. It
/// resolves fail-closed — a quote currency with no rate raises rather than
/// storing an unconverted number — so there is no non-USD row to guard
/// against. A DECLARED field of the wrong shape fails the whole row, which
/// the caller then skips.
#[derive(Deserialize)]
struct RawRow {
    #[serde(default)]
    rates: Option<RawRates>,
    /// Held as raw values so a malformed entry skips only itself; a typed
    /// `Vec<RawOverride>` would fail the whole row over one bad entry.
    #[serde(default)]
    overrides: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    effective_at: Option<String>,
    /// Present = the row is delisted (the index's removal convention keeps
    /// the row with its last prices and stamps it); the caller skips it, so
    /// it prices nothing at any date. The history spells a delisting
    /// `removed: true` instead.
    #[serde(default)]
    removed_at: Option<String>,
}

/// One history-ndjson line: the row shape plus the line's own bookkeeping
/// keys. `observed_at` is mandatory — a row with no date cannot date.
#[derive(Deserialize)]
struct RawHistoryRow {
    source: String,
    model_id: String,
    observed_at: String,
    #[serde(default)]
    removed: Option<bool>,
    #[serde(flatten)]
    row: RawRow,
}

/// One row's entries: the flat base entry plus the row's override entries, in
/// order — later entries override earlier matching ones at selection time.
fn row_entries(base: RawRates, overrides: Option<Vec<serde_json::Value>>) -> Vec<PriceEntry> {
    let mut prices = vec![base.entry(&RawRates::default(), None)];
    for raw in overrides.into_iter().flatten() {
        let Ok(entry) = serde_json::from_value::<RawOverride>(raw) else {
            continue; // malformed entry — skip, don't fail the row
        };
        if let Some(entry) = entry.to_entry(&base) {
            prices.push(entry);
        }
    }
    prices
}

/// The priced model an entry set names, or `None` for a model with no input
/// AND no output rate anywhere (per-request / web-search pricing): keeping it
/// would render a $0 "priced" row instead of an unpriced dash.
fn finish_priced(
    id: String,
    prices: Vec<PriceEntry>,
    effective_at: Option<String>,
) -> Option<PricedModel> {
    if !prices.iter().any(|e| e.input != 0.0 || e.output != 0.0) {
        return None;
    }
    Some(PricedModel {
        id,
        prices,
        effective_at,
    })
}

impl RawRow {
    /// Distill a row into its priced model: the flat base entry plus the
    /// override entries. One shape serves both feeds — the index and the
    /// history spell a row identically past their own bookkeeping keys.
    fn into_priced(self, id: String) -> Option<PricedModel> {
        let prices = row_entries(self.rates.unwrap_or_default(), self.overrides);
        finish_priced(id, prices, self.effective_at)
    }
}

/// USD-per-million → per-token.
fn to_per_token(mtok: f64) -> f64 {
    mtok / 1e6
}

// ── Disk cache ───────────────────────────────────────────────────────────────

/// On-disk cache shape: the fetch time, the store-derived dating source, the
/// catalog's two distilled halves, and the local snapshot history. A cache
/// written before the store, aliases, or canonical half exists loads with
/// that half empty (serde default) and upgrades on the next successful fetch.
#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    fetched_at_ms: u64,
    #[serde(default)]
    store: Vec<StoreKey>,
    #[serde(default)]
    aliases: Vec<AliasKey>,
    #[serde(default)]
    canonical: CanonicalMap,
    #[serde(default)]
    history: Vec<RateSnapshot>,
}

/// `~/.clauth/ai_pricelog_v4_price_cache.json`. Resolved ONCE at spawn time
/// and passed into the worker so the detached thread never re-resolves
/// `home_dir()` later.
///
/// The name carries the feed generation on purpose. A v3-era cache parses into
/// these types unchanged — they are clauth's own distilled shapes — but its
/// `store` was filtered by the retired provider allowlist, so reading it would
/// serve the very rows this build exists to drop (and dash the ones it adds)
/// until the next fetch. [`first_delay`] does not force that fetch: it returns
/// the interval MINUS the cache age, so a cache written an hour ago would
/// delay the first v4 fetch by 23 more. Renaming makes the upgrade a cold
/// cache, which `first_delay` ticks immediately, and
/// [`delete_stale_cache_once`] sweeps the old file the way the two
/// pre-ai-pricelog names were swept.
fn cache_path() -> Option<PathBuf> {
    clauth_dir()
        .ok()
        .map(|d| d.join("ai_pricelog_v4_price_cache.json"))
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
        store: cache.store,
        aliases: cache.aliases,
        canonical: cache.canonical,
        fetched_at_ms: cache.fetched_at_ms,
        memo: Mutex::default(),
    })
}

/// Persist the cache best-effort (atomic tmp + rename). Errors are swallowed.
fn save_cache(path: &Path, table: &PriceTable) {
    let cache = CacheFile {
        fetched_at_ms: table.fetched_at_ms,
        store: table.store.clone(),
        aliases: table.aliases.clone(),
        canonical: table.canonical.clone(),
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
