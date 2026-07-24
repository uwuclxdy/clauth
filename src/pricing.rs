//! Model price table — fetches per-token USD rates so the Tokens tab can show
//! API-equivalent cost (what the recorded usage *would* cost at pay-as-you-go
//! API rates; clauth users are on subscription plans, so this reads as "value
//! extracted", not a bill).
//!
//! # Source
//!
//! LiteLLM's community-maintained `model_prices_and_context_window.json`
//! (<https://github.com/BerriAI/litellm>). It is machine-readable, keyed by raw
//! model id, and carries the four rates we need per model — input, output,
//! cache-read, and cache-write — so the cache multipliers (read ≈ 0.1×, 5-minute
//! write = 1.25× of input) are encoded as concrete per-token costs rather than
//! assumed. It covers first-party Anthropic ids (bare and date-stamped),
//! `claude-fable-5`, and third-party providers clauth recognizes (e.g. DeepSeek).
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
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::poll::{first_delay, run_polling_loop};
use crate::profile::{atomic_write_600, clauth_dir};
use crate::tokens::ModelTokens;
use crate::usage::now_ms;

/// Live price feed (LiteLLM machine-readable pricing JSON).
const FEED_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Background refresh cadence. Prices move rarely, so this is deliberately slow;
/// a manual refresh signal short-circuits the wait.
const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// HTTP connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// HTTP response-receive timeout. In ureq 3 this deadline keeps counting
/// through the BODY read (pinned by the proxy's timeout-semantics tests), so
/// it must cover the whole ~1.5 MiB feed on a slow link, not just the
/// headers — 15 s needed ≥100 KB/s sustained and truncated the fetch on
/// throttled connections (timeout-sweep 2026-07-18). Background thread; a
/// longer bound costs nothing.
const RECV_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard cap on the response body. The real feed is ~1.5 MiB; 8 MiB is generous
/// headroom while still bounding a hostile / runaway response.
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

// ── Public data model ─────────────────────────────────────────────────────────

/// Per-token USD rates for one model. `cache_write` is the 5-minute-TTL creation
/// rate (the common case; the 1-hour rate is not modeled — stats-cache does not
/// record TTL). Missing upstream fields (e.g. a provider with no cache-write
/// rate) default to `0.0`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct ModelRate {
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cache_read: f64,
    pub(crate) cache_write: f64,
}

/// Resolved price table: raw model id → per-token rates, plus the wall-clock time
/// it was fetched (for a freshness badge).
#[derive(Debug, Clone)]
pub(crate) struct PriceTable {
    rates: HashMap<String, ModelRate>,
    pub(crate) fetched_at_ms: u64,
}

#[cfg(test)]
impl PriceTable {
    /// Literal table for tests outside this module — keeps `rates` private
    /// (lookups stay funneled through `rate`/`cost`/`total_cost`).
    pub(crate) fn from_rates(rates: HashMap<String, ModelRate>) -> Self {
        Self {
            rates,
            fetched_at_ms: 0,
        }
    }
}

impl PriceTable {
    /// Rate for a model id, with suffix fallback. Tries an exact match, then
    /// progressively strips trailing `-<segment>` groups — so a date stamp
    /// (`claude-sonnet-4-5-20250929`) or a variant suffix (`claude-opus-4-6-thinking`)
    /// falls back to the base id (`claude-sonnet-4-5` / `claude-opus-4-6`). Only
    /// suffixes are stripped, never the family prefix, so it never cross-matches a
    /// different model. The fallback never matches a **bare single-token** key
    /// (one with no `-`, e.g. `claude`): such a key would wildcard-match every
    /// variant of the family, so it is only ever reachable via an exact match.
    pub(crate) fn rate(&self, model: &str) -> Option<ModelRate> {
        if let Some(r) = self.rates.get(model) {
            return Some(*r);
        }
        let mut cur = model;
        while let Some((head, _)) = cur.rsplit_once('-') {
            if head.contains('-')
                && let Some(r) = self.rates.get(head)
            {
                return Some(*r);
            }
            cur = head;
        }
        None
    }

    /// API-equivalent cost in USD for one model's recorded tokens. `None` when no
    /// rate matches (unknown / unpriced model). Counts all four token buckets.
    pub(crate) fn cost(&self, m: &ModelTokens) -> Option<f64> {
        let r = self.rate(&m.model)?;
        Some(
            m.input as f64 * r.input
                + m.output as f64 * r.output
                + m.cache_read as f64 * r.cache_read
                + m.cache_create as f64 * r.cache_write,
        )
    }

    /// Summed cost over a slice of models. Returns `(priced_total_usd,
    /// unpriced_count)` — `unpriced_count` is how many had nonzero tokens but no
    /// matching rate, so the UI can flag that the figure is a floor.
    pub(crate) fn total_cost(&self, models: &[ModelTokens]) -> (f64, usize) {
        let mut total = 0.0;
        let mut unpriced = 0usize;
        for m in models {
            match self.cost(m) {
                Some(c) => total += c,
                None if m.total() > 0 => unpriced += 1,
                None => {}
            }
        }
        (total, unpriced)
    }
}

// ── Background thread ──────────────────────────────────────────────────────────

/// Events emitted by the background pricing worker.
pub(crate) enum PricingEvent {
    /// A fresh or cached table is available.
    Loaded(Box<PriceTable>),
    /// A fetch failed and no cache was available. UI keeps showing `—`.
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
        run_polling_loop(&refresh_rx, first, REFRESH_INTERVAL, || {
            run_fetch(&tx, &cache_file)
        });
    });
}

/// One fetch attempt. On success: distill, cache, send `Loaded`. On failure:
/// fall back to the cache when one exists (`Loaded`); only when nothing is cached
/// do we surface `Failed`.
fn run_fetch(tx: &Sender<PricingEvent>, cache_file: &Path) {
    match fetch_table() {
        Ok(mut table) => {
            table.fetched_at_ms = now_ms();
            save_cache(cache_file, &table);
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

/// Fetch and distill the live feed. The body is capped at [`MAX_BODY_BYTES`].
fn fetch_table() -> anyhow::Result<PriceTable> {
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
    let rates = distill(&json)?;
    if rates.is_empty() {
        anyhow::bail!("price feed parsed but contained no priced models");
    }
    Ok(PriceTable {
        rates,
        fetched_at_ms: 0, // stamped by the caller on success
    })
}

/// Parse the LiteLLM JSON into a flat `id → ModelRate` map. Tolerant: a value
/// that is not an object, or lacks `input_cost_per_token`, is skipped rather
/// than failing the whole parse. Path-style keys (`bedrock/`, `azure_ai/`) are
/// dropped — Claude Code logs bare model ids, so keeping them only bloats the
/// cache. Missing cache rates default to `0.0`.
fn distill(json: &str) -> anyhow::Result<HashMap<String, ModelRate>> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(anyhow::Error::from)?;
    let obj = root
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("price feed root is not a JSON object"))?;

    let mut rates = HashMap::new();
    for (id, v) in obj {
        if id.contains('/') {
            continue;
        }
        let Some(input) = v
            .get("input_cost_per_token")
            .and_then(serde_json::Value::as_f64)
        else {
            continue;
        };
        let f = |key: &str| {
            v.get(key)
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0)
        };
        rates.insert(
            id.clone(),
            ModelRate {
                input,
                output: f("output_cost_per_token"),
                cache_read: f("cache_read_input_token_cost"),
                cache_write: f("cache_creation_input_token_cost"),
            },
        );
    }
    Ok(rates)
}

// ── Disk cache ──────────────────────────────────────────────────────────────

/// On-disk cache shape: the distilled rates plus the time they were fetched.
#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    fetched_at_ms: u64,
    rates: HashMap<String, ModelRate>,
}

/// `~/.clauth/price_cache.json`. Resolved ONCE at spawn time and passed into the
/// worker so the detached thread never re-resolves `home_dir()` later.
fn cache_path() -> Option<PathBuf> {
    clauth_dir().ok().map(|d| d.join("price_cache.json"))
}

/// Synchronous one-shot read of the on-disk price cache, off the background
/// [`spawn`] channel — the CLI sessions surface needs a `PriceTable` on the main
/// thread without standing up the worker. `None` when the cache is absent or
/// unparseable; never fetches, so a cold cache simply prices nothing.
#[allow(
    dead_code,
    reason = "sync price path for the sessions CLI, wired by a later phase"
)]
pub(crate) fn load_cached() -> Option<PriceTable> {
    load_cache(&cache_path()?)
}

/// Load the cache if it exists and parses; `None` on any miss/error (a stale or
/// reshaped cache is silently treated as no cache).
fn load_cache(path: &Path) -> Option<PriceTable> {
    let bytes = std::fs::read_to_string(path).ok()?;
    let cache: CacheFile = serde_json::from_str(&bytes).ok()?;
    Some(PriceTable {
        rates: cache.rates,
        fetched_at_ms: cache.fetched_at_ms,
    })
}

/// Persist the cache best-effort (atomic tmp + rename). Errors are swallowed.
fn save_cache(path: &Path, table: &PriceTable) {
    let cache = CacheFile {
        fetched_at_ms: table.fetched_at_ms,
        rates: table.rates.clone(),
    };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = atomic_write_600(path, json);
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "../tests/inline/pricing.rs"]
mod tests;
